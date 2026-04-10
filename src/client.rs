use crate::protocol::{self, Request, Response};
use anyhow::{Context, Result};
use nix::sys::termios;
use std::collections::HashMap;
use std::path::Path;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

/// RAII guard that restores terminal on drop (handles panic + signals).
struct RawModeGuard(termios::Termios);

impl RawModeGuard {
    fn enter() -> Result<Self> {
        let stdin = std::io::stdin();
        let orig = termios::tcgetattr(&stdin).context("tcgetattr failed")?;
        let mut raw = orig.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(&stdin, termios::SetArg::TCSANOW, &raw)
            .context("tcsetattr failed")?;
        Ok(Self(orig))
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let stdin = std::io::stdin();
        let _ = termios::tcsetattr(&stdin, termios::SetArg::TCSANOW, &self.0);
    }
}

async fn send_request(stream: &mut UnixStream, req: &Request) -> Result<()> {
    let json = serde_json::to_vec(req)?;
    let frame = protocol::encode(&json);
    stream.write_all(&frame).await?;
    Ok(())
}

async fn read_response(stream: &mut UnixStream) -> Result<Option<Response>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1024 * 1024 {
        anyhow::bail!("Frame too large: {len}");
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    let resp: Response = serde_json::from_slice(&buf)?;
    Ok(Some(resp))
}

pub async fn spawn_and_attach(
    socket_path: &Path,
    command: &str,
    args: &[String],
    env: Option<HashMap<String, String>>,
    ready_pattern: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    let (default_cols, default_rows) = term_size().unwrap_or((80, 24));
    send_request(
        &mut stream,
        &Request::Spawn {
            command: command.to_string(),
            args: args.to_vec(),
            cols: Some(cols.unwrap_or(default_cols)),
            rows: Some(rows.unwrap_or(default_rows)),
            env,
            ready_pattern,
            name: None,
        },
    )
    .await?;

    let resp = read_response(&mut stream)
        .await?
        .context("No response from daemon")?;
    match resp {
        Response::Spawned { session_id } => {
            info!("Spawned session {session_id}, attaching...");
        }
        Response::Error { message } => anyhow::bail!("Spawn failed: {message}"),
        _ => anyhow::bail!("Unexpected response"),
    }

    run_attach_loop(stream).await
}

pub async fn attach(socket_path: &Path, session_id: u32) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    send_request(&mut stream, &Request::Attach { session_id }).await?;

    let resp = read_response(&mut stream)
        .await?
        .context("No response from daemon")?;
    match resp {
        Response::Attached { session_id } => {
            info!("Attached to session {session_id}");
        }
        Response::Error { message } => anyhow::bail!("Attach failed: {message}"),
        _ => anyhow::bail!("Unexpected response"),
    }

    run_attach_loop(stream).await
}

pub async fn list_sessions(socket_path: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    send_request(&mut stream, &Request::List).await?;

    let resp = read_response(&mut stream)
        .await?
        .context("No response from daemon")?;
    match resp {
        Response::Sessions { sessions } => {
            if sessions.is_empty() {
                println!("No active sessions.");
            } else {
                println!(
                    "{:<6} {:<14} {:<10} {:<6} {:<10} {}",
                    "ID", "NAME", "STATUS", "READY", "SIZE", "COMMAND"
                );
                for s in sessions {
                    let name = s.name.as_deref().unwrap_or("-");
                    let status = if s.running {
                        "running".to_string()
                    } else {
                        format!(
                            "exit({})",
                            s.exit_code
                                .map(|c| c.to_string())
                                .unwrap_or("?".into())
                        )
                    };
                    let ready = if s.ready { "yes" } else { "no" };
                    let size = format!("{}x{}", s.cols, s.rows);
                    println!(
                        "{:<6} {:<14} {:<10} {:<6} {:<10} {}",
                        s.id, name, status, ready, size, s.command
                    );
                }
            }
        }
        _ => anyhow::bail!("Unexpected response"),
    }

    Ok(())
}

pub async fn create_instance(
    socket_path: &Path,
    name: &str,
    command: &str,
    args: &[String],
    env: Option<HashMap<String, String>>,
    working_directory: Option<String>,
    topic_name: Option<String>,
    ready_pattern: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    send_request(
        &mut stream,
        &Request::CreateInstance {
            name: name.to_string(),
            command: command.to_string(),
            args: args.to_vec(),
            env,
            working_directory,
            topic_name,
            ready_pattern,
            cols,
            rows,
        },
    )
    .await?;

    let resp = read_response(&mut stream)
        .await?
        .context("No response from daemon")?;
    match resp {
        Response::InstanceCreated {
            name,
            session_id,
            topic_id,
        } => {
            println!(
                "Created instance '{name}' (session {session_id}{})",
                topic_id
                    .map(|t| format!(", topic {t}"))
                    .unwrap_or_default()
            );
        }
        Response::Error { message } => anyhow::bail!("Create instance failed: {message}"),
        _ => anyhow::bail!("Unexpected response"),
    }

    Ok(())
}

pub async fn inject(socket_path: &Path, session_id: u32, data: &[u8]) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    send_request(
        &mut stream,
        &Request::Inject {
            session_id,
            data: data.to_vec(),
        },
    )
    .await?;

    let resp = read_response(&mut stream)
        .await?
        .context("No response from daemon")?;
    match resp {
        Response::Injected {
            session_id,
            bytes_written,
        } => {
            println!("Injected {bytes_written} bytes into session {session_id}");
        }
        Response::Error { message } => anyhow::bail!("Inject failed: {message}"),
        _ => anyhow::bail!("Unexpected response"),
    }

    Ok(())
}

pub async fn kill_session(
    socket_path: &Path,
    session_id: u32,
    quit_command: Option<String>,
    grace_seconds: Option<u32>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    send_request(
        &mut stream,
        &Request::Kill {
            session_id,
            quit_command,
            grace_seconds,
        },
    )
    .await?;

    let resp = read_response(&mut stream)
        .await?
        .context("No response from daemon")?;
    match resp {
        Response::Killed { session_id } => {
            println!("Session {session_id} killed");
        }
        Response::Error { message } => anyhow::bail!("Kill failed: {message}"),
        _ => anyhow::bail!("Unexpected response"),
    }

    Ok(())
}

pub async fn fleet_start(
    socket_path: &Path,
    config_path: &str,
    names: Vec<String>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    send_request(
        &mut stream,
        &Request::FleetStart {
            config_path: config_path.to_string(),
            names,
        },
    )
    .await?;

    let resp = read_response(&mut stream)
        .await?
        .context("No response from daemon")?;
    match resp {
        Response::FleetStarted { started } => {
            println!("Started {} instance(s): {}", started.len(), started.join(", "));
        }
        Response::Error { message } => anyhow::bail!("Fleet start failed: {message}"),
        _ => anyhow::bail!("Unexpected response"),
    }

    Ok(())
}

pub async fn fleet_stop(socket_path: &Path, names: Vec<String>) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    send_request(&mut stream, &Request::FleetStop { names }).await?;

    let resp = read_response(&mut stream)
        .await?
        .context("No response from daemon")?;
    match resp {
        Response::FleetStopped { stopped } => {
            println!("Stopped {} instance(s): {}", stopped.len(), stopped.join(", "));
        }
        Response::Error { message } => anyhow::bail!("Fleet stop failed: {message}"),
        _ => anyhow::bail!("Unexpected response"),
    }

    Ok(())
}

pub async fn reply(socket_path: &Path, session_id: u32, text: &str) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    send_request(
        &mut stream,
        &Request::Reply {
            session_id,
            text: text.to_string(),
        },
    )
    .await?;

    let resp = read_response(&mut stream)
        .await?
        .context("No response from daemon")?;
    match resp {
        Response::Sent => {}
        Response::Error { message } => anyhow::bail!("Reply failed: {message}"),
        _ => anyhow::bail!("Unexpected response"),
    }

    Ok(())
}

pub async fn send_message(
    socket_path: &Path,
    session_id: u32,
    target: &str,
    text: &str,
    kind: Option<String>,
    correlation_id: Option<String>,
) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    send_request(
        &mut stream,
        &Request::SendMessage {
            session_id,
            target: target.to_string(),
            text: text.to_string(),
            kind,
            correlation_id,
        },
    )
    .await?;

    let resp = read_response(&mut stream)
        .await?
        .context("No response from daemon")?;
    match resp {
        Response::Sent => {}
        Response::Error { message } => anyhow::bail!("Send failed: {message}"),
        _ => anyhow::bail!("Unexpected response"),
    }

    Ok(())
}

pub async fn inbox(socket_path: &Path, session_id: u32) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    send_request(&mut stream, &Request::Inbox { session_id }).await?;

    let resp = read_response(&mut stream)
        .await?
        .context("No response from daemon")?;
    match resp {
        Response::Messages { messages } => {
            if messages.is_empty() {
                println!("No pending messages.");
            } else {
                for msg in &messages {
                    let kind_str = msg
                        .kind
                        .as_deref()
                        .map(|k| format!(" ({k})"))
                        .unwrap_or_default();
                    println!("[from:{}]{} {}", msg.from, kind_str, msg.text);
                }
                println!("---\n{} message(s) total.", messages.len());
            }
        }
        Response::Error { message } => anyhow::bail!("Inbox failed: {message}"),
        _ => anyhow::bail!("Unexpected response"),
    }

    Ok(())
}

async fn run_attach_loop(stream: UnixStream) -> Result<()> {
    let _guard = RawModeGuard::enter()?;
    let result = attach_bridge(stream).await;
    println!();
    result
}

async fn attach_bridge(stream: UnixStream) -> Result<()> {
    let (mut stream_reader, stream_writer) = stream.into_split();
    let stream_writer = std::sync::Arc::new(tokio::sync::Mutex::new(stream_writer));
    let mut stdout = io::stdout();

    // Send initial resize
    if let Some((cols, rows)) = term_size() {
        let req = Request::Resize { cols, rows };
        let json = serde_json::to_vec(&req).expect("Resize serialization is infallible");
        let frame = protocol::encode(&json);
        let _ = stream_writer.lock().await.write_all(&frame).await;
    }

    // SIGWINCH handler
    let sw = stream_writer.clone();
    let winch_handle = tokio::spawn(async move {
        let mut sigwinch = match signal(SignalKind::window_change()) {
            Ok(s) => s,
            Err(_) => return,
        };
        while sigwinch.recv().await.is_some() {
            if let Some((cols, rows)) = term_size() {
                let req = Request::Resize { cols, rows };
                let json = serde_json::to_vec(&req).expect("Resize serialization is infallible");
                let frame = protocol::encode(&json);
                let _ = sw.lock().await.write_all(&frame).await;
            }
        }
    });

    // Task: stdin → daemon (with Ctrl+B d detection)
    let sw = stream_writer.clone();
    let mut stdin_handle = tokio::spawn(async move {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 1024];
        let mut ctrl_b_pressed = false;

        loop {
            let n = match stdin.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };

            let mut i = 0;
            while i < n {
                if ctrl_b_pressed {
                    ctrl_b_pressed = false;
                    if buf[i] == b'd' {
                        let req = Request::Detach;
                        let json =
                            serde_json::to_vec(&req).expect("Detach serialization is infallible");
                        let frame = protocol::encode(&json);
                        let _ = sw.lock().await.write_all(&frame).await;
                        return true;
                    }
                    let data = vec![0x02, buf[i]];
                    let req = Request::Write { data };
                    let json =
                        serde_json::to_vec(&req).expect("Write serialization is infallible");
                    let frame = protocol::encode(&json);
                    if sw.lock().await.write_all(&frame).await.is_err() {
                        return false;
                    }
                    i += 1;
                } else if buf[i] == 0x02 {
                    ctrl_b_pressed = true;
                    i += 1;
                } else {
                    let start = i;
                    while i < n && buf[i] != 0x02 {
                        i += 1;
                    }
                    let req = Request::Write {
                        data: buf[start..i].to_vec(),
                    };
                    let json =
                        serde_json::to_vec(&req).expect("Write serialization is infallible");
                    let frame = protocol::encode(&json);
                    if sw.lock().await.write_all(&frame).await.is_err() {
                        return false;
                    }
                }
            }
        }
        false
    });

    // Task: daemon output → stdout
    let mut output_handle = tokio::spawn(async move {
        let mut len_buf = [0u8; 4];
        loop {
            if stream_reader.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            if len > 1024 * 1024 {
                break;
            }
            let mut buf = vec![0u8; len];
            if stream_reader.read_exact(&mut buf).await.is_err() {
                break;
            }
            let resp: Response = match serde_json::from_slice(&buf) {
                Ok(r) => r,
                Err(_) => break,
            };
            match resp {
                Response::Output { data } => {
                    if stdout.write_all(&data).await.is_err() {
                        break;
                    }
                    let _ = stdout.flush().await;
                }
                Response::SessionExited {
                    session_id,
                    exit_code,
                } => {
                    eprintln!(
                        "\r\n[Session {session_id} exited with code {}]",
                        exit_code.map(|c| c.to_string()).unwrap_or("?".into())
                    );
                    break;
                }
                Response::Detached => {
                    eprintln!("\r\n[Detached]");
                    break;
                }
                _ => {}
            }
        }
    });

    // Also listen for SIGINT/SIGTERM to ensure clean exit from raw mode
    let mut sigint = signal(SignalKind::interrupt()).ok();
    let mut sigterm = signal(SignalKind::terminate()).ok();

    tokio::select! {
        _ = &mut stdin_handle => { output_handle.abort(); },
        _ = &mut output_handle => { stdin_handle.abort(); },
        _ = async { if let Some(ref mut s) = sigint { s.recv().await } else { std::future::pending().await } } => {
            stdin_handle.abort();
            output_handle.abort();
            eprintln!("\r\n[Interrupted]");
        },
        _ = async { if let Some(ref mut s) = sigterm { s.recv().await } else { std::future::pending().await } } => {
            stdin_handle.abort();
            output_handle.abort();
            eprintln!("\r\n[Terminated]");
        },
    }
    winch_handle.abort();

    Ok(())
}

fn term_size() -> Option<(u16, u16)> {
    use nix::libc::{ioctl, winsize, TIOCGWINSZ};
    use std::os::fd::AsRawFd;
    let mut ws: winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { ioctl(std::io::stdout().as_raw_fd(), TIOCGWINSZ as _, &mut ws) };
    if ret == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        Some((ws.ws_col, ws.ws_row))
    } else {
        None
    }
}
