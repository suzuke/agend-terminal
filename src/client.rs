use crate::protocol::{self, Request, Response};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use std::collections::HashMap;
use std::path::Path;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::info;

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

    let (default_cols, default_rows) = terminal::size().unwrap_or((80, 24));
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
    terminal::enable_raw_mode().context("Failed to enable raw mode")?;
    let result = attach_bridge(stream).await;
    terminal::disable_raw_mode().ok();
    println!();
    result
}

async fn attach_bridge(stream: UnixStream) -> Result<()> {
    let (mut stream_reader, stream_writer) = stream.into_split();
    let stream_writer = std::sync::Arc::new(tokio::sync::Mutex::new(stream_writer));
    let mut stdout = io::stdout();

    // Send initial resize
    if let Ok((cols, rows)) = terminal::size() {
        let req = Request::Resize { cols, rows };
        let json = serde_json::to_vec(&req).expect("Resize serialization is infallible");
        let frame = protocol::encode(&json);
        let _ = stream_writer.lock().await.write_all(&frame).await;
    }

    // Input task: crossterm events → daemon
    // Uses spawn_blocking because crossterm::event::read() is blocking
    let sw = stream_writer.clone();
    let mut input_handle = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Handle::current();
        loop {
            // Poll with timeout to allow task cancellation
            if !event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
                continue;
            }
            match event::read() {
                Ok(Event::Key(KeyEvent { code, modifiers, .. })) => {
                    // Ctrl+D to detach
                    if code == KeyCode::Char('d') && modifiers.contains(KeyModifiers::CONTROL) {
                        let sw = sw.clone();
                        rt.block_on(async {
                            let req = Request::Detach;
                            let json = serde_json::to_vec(&req).expect("infallible");
                            let frame = protocol::encode(&json);
                            let _ = sw.lock().await.write_all(&frame).await;
                        });
                        return true; // detached
                    }
                    let bytes = key_to_bytes(code, modifiers);
                    if !bytes.is_empty() {
                        let sw = sw.clone();
                        let ok = rt.block_on(async {
                            let req = Request::Write { data: bytes };
                            let json = serde_json::to_vec(&req).expect("infallible");
                            let frame = protocol::encode(&json);
                            sw.lock().await.write_all(&frame).await.is_ok()
                        });
                        if !ok { return false; }
                    }
                }
                Ok(Event::Paste(text)) => {
                    let sw = sw.clone();
                    let ok = rt.block_on(async {
                        let req = Request::Write { data: text.into_bytes() };
                        let json = serde_json::to_vec(&req).expect("infallible");
                        let frame = protocol::encode(&json);
                        sw.lock().await.write_all(&frame).await.is_ok()
                    });
                    if !ok { return false; }
                }
                Ok(Event::Resize(cols, rows)) => {
                    let sw = sw.clone();
                    rt.block_on(async {
                        let req = Request::Resize { cols, rows };
                        let json = serde_json::to_vec(&req).expect("infallible");
                        let frame = protocol::encode(&json);
                        let _ = sw.lock().await.write_all(&frame).await;
                    });
                }
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    });

    // Output task: daemon → stdout
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

    tokio::select! {
        _ = &mut input_handle => { output_handle.abort(); },
        _ = &mut output_handle => { input_handle.abort(); },
    }

    Ok(())
}

/// Convert crossterm KeyEvent to terminal bytes.
fn key_to_bytes(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let alt = modifiers.contains(KeyModifiers::ALT);
    match code {
        KeyCode::Char(c) if ctrl => vec![(c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1)],
        KeyCode::Char(c) if alt => {
            let mut v = vec![0x1b];
            let mut b = [0u8; 4];
            v.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            v
        }
        KeyCode::Char(c) => {
            let mut b = [0u8; 4];
            c.encode_utf8(&mut b).as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => vec![],
        },
        _ => vec![],
    }
}
