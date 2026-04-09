use crate::protocol::{self, Request, Response};
use anyhow::{Context, Result};
use nix::sys::termios;
use std::path::Path;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::info;

/// Enter raw mode and return the original termios for restoration.
fn enter_raw_mode() -> Result<termios::Termios> {
    let stdin = std::io::stdin();
    let orig = termios::tcgetattr(&stdin).context("tcgetattr failed")?;
    let mut raw = orig.clone();
    termios::cfmakeraw(&mut raw);
    termios::tcsetattr(&stdin, termios::SetArg::TCSANOW, &raw)
        .context("tcsetattr failed")?;
    Ok(orig)
}

fn restore_terminal(orig: &termios::Termios) {
    let stdin = std::io::stdin();
    let _ = termios::tcsetattr(&stdin, termios::SetArg::TCSANOW, orig);
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

pub async fn spawn_and_attach(socket_path: &Path, command: &str, args: &[String]) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon. Is it running?")?;

    send_request(
        &mut stream,
        &Request::Spawn {
            command: command.to_string(),
            args: args.to_vec(),
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
                println!("{:<6} {:<10} {}", "ID", "STATUS", "COMMAND");
                for s in sessions {
                    let status = if s.running { "running" } else { "exited" };
                    println!("{:<6} {:<10} {}", s.id, status, s.command);
                }
            }
        }
        _ => anyhow::bail!("Unexpected response"),
    }

    Ok(())
}

/// Main attach loop: bridge terminal stdin/stdout to daemon.
async fn run_attach_loop(stream: UnixStream) -> Result<()> {
    // Enter raw mode
    let orig_termios = enter_raw_mode()?;

    let result = attach_bridge(stream).await;
    restore_terminal(&orig_termios);
    println!();

    result
}

async fn attach_bridge(stream: UnixStream) -> Result<()> {
    let (mut stream_reader, mut stream_writer) = stream.into_split();
    let mut stdout = io::stdout();

    let (detach_tx, mut detach_rx) = tokio::sync::oneshot::channel::<()>();
    let detach_tx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(detach_tx)));

    // Send initial resize
    if let Some((cols, rows)) = term_size() {
        let req = Request::Resize { cols, rows };
        let json = serde_json::to_vec(&req).unwrap();
        let frame = protocol::encode(&json);
        let _ = stream_writer.write_all(&frame).await;
    }

    // Task: stdin → daemon (with Ctrl+B d detection)
    let stdin_handle = tokio::spawn(async move {
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
                        // Detach
                        let req = Request::Detach;
                        let json = serde_json::to_vec(&req).unwrap();
                        let frame = protocol::encode(&json);
                        let _ = stream_writer.write_all(&frame).await;
                        if let Some(tx) = detach_tx.lock().await.take() {
                            let _ = tx.send(());
                        }
                        return;
                    }
                    // Not 'd' — send Ctrl+B + current byte
                    let data = vec![0x02, buf[i]];
                    let req = Request::Write { data };
                    let json = serde_json::to_vec(&req).unwrap();
                    let frame = protocol::encode(&json);
                    if stream_writer.write_all(&frame).await.is_err() {
                        return;
                    }
                    i += 1;
                } else if buf[i] == 0x02 {
                    ctrl_b_pressed = true;
                    i += 1;
                } else {
                    // Batch contiguous normal bytes
                    let start = i;
                    while i < n && buf[i] != 0x02 {
                        i += 1;
                    }
                    let req = Request::Write {
                        data: buf[start..i].to_vec(),
                    };
                    let json = serde_json::to_vec(&req).unwrap();
                    let frame = protocol::encode(&json);
                    if stream_writer.write_all(&frame).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    // Task: daemon output → stdout
    let output_handle = tokio::spawn(async move {
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
        _ = stdin_handle => {},
        _ = output_handle => {},
        _ = &mut detach_rx => {},
    }

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
