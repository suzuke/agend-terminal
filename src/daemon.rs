use crate::protocol::{self, Request, Response, SessionInfo};
use crate::pty_session::PtySession;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{error, info};

struct DaemonState {
    sessions: HashMap<u32, Arc<PtySession>>,
    next_id: u32,
    log_dir: PathBuf,
}

impl DaemonState {
    fn new(log_dir: PathBuf) -> Self {
        Self {
            sessions: HashMap::new(),
            next_id: 1,
            log_dir,
        }
    }
}

pub async fn run(socket_path: &Path) -> Result<()> {
    if socket_path.exists() {
        if UnixStream::connect(socket_path).await.is_ok() {
            anyhow::bail!(
                "Another daemon is already running on {:?}",
                socket_path
            );
        }
        std::fs::remove_file(socket_path)?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let log_dir = socket_path
        .parent()
        .unwrap_or(Path::new("/tmp"))
        .join("sessions");
    std::fs::create_dir_all(&log_dir)?;

    let listener = UnixListener::bind(socket_path).context("Failed to bind UDS")?;
    info!("Daemon listening on {:?}", socket_path);
    info!("Session logs: {:?}", log_dir);

    let state = Arc::new(Mutex::new(DaemonState::new(log_dir)));

    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, state).await {
                error!("Client handler error: {e:#}");
            }
        });
    }
}

async fn send_response(stream: &mut UnixStream, resp: &Response) -> Result<()> {
    let json = serde_json::to_vec(resp)?;
    let frame = protocol::encode(&json);
    stream.write_all(&frame).await?;
    Ok(())
}

async fn read_request(stream: &mut UnixStream) -> Result<Option<Request>> {
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
    let req: Request = serde_json::from_slice(&buf)?;
    Ok(Some(req))
}

async fn handle_client(mut stream: UnixStream, state: Arc<Mutex<DaemonState>>) -> Result<()> {
    let req = match read_request(&mut stream).await? {
        Some(r) => r,
        None => return Ok(()),
    };

    match req {
        Request::Spawn {
            command,
            args,
            cols,
            rows,
            env,
            ready_pattern,
        } => {
            let cols = cols.unwrap_or(80);
            let rows = rows.unwrap_or(24);
            let (id, log_dir) = {
                let mut st = state.lock().await;
                let id = st.next_id;
                st.next_id += 1;
                (id, st.log_dir.clone())
            };
            let session = Arc::new(PtySession::spawn(
                id,
                &command,
                &args,
                cols,
                rows,
                env.as_ref(),
                ready_pattern.as_deref(),
                &log_dir,
            )?);
            {
                let mut st = state.lock().await;
                st.sessions.insert(id, session.clone());
            }
            spawn_session_reaper(id, session.clone(), state.clone());
            info!("Spawned session {id}: {command}");
            send_response(&mut stream, &Response::Spawned { session_id: id }).await?;
            attach_loop(stream, session).await?;
        }
        Request::Attach { session_id } => {
            let session = {
                let st = state.lock().await;
                st.sessions.get(&session_id).cloned()
            };
            match session {
                Some(session) => {
                    send_response(&mut stream, &Response::Attached { session_id }).await?;
                    attach_loop(stream, session).await?;
                }
                None => {
                    send_response(
                        &mut stream,
                        &Response::Error {
                            message: format!("Session {session_id} not found"),
                        },
                    )
                    .await?;
                }
            }
        }
        Request::List => {
            let st = state.lock().await;
            let mut sessions = Vec::new();
            for (_, s) in &st.sessions {
                let running = s.is_running().await;
                let (cols, rows) = s.get_size().await;
                sessions.push(SessionInfo {
                    id: s.id,
                    command: s.command.clone(),
                    running,
                    exit_code: s.get_exit_code().await,
                    ready: s.is_ready(),
                    cols,
                    rows,
                });
            }
            send_response(&mut stream, &Response::Sessions { sessions }).await?;
        }
        Request::Inject { session_id, data } => {
            let session = {
                let st = state.lock().await;
                st.sessions.get(&session_id).cloned()
            };
            match session {
                Some(session) => {
                    if !session.is_running().await {
                        send_response(
                            &mut stream,
                            &Response::Error {
                                message: format!("Session {session_id} has exited"),
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                    let len = data.len();
                    match session.write_input(&data).await {
                        Ok(()) => {
                            info!("Injected {len} bytes into session {session_id}");
                            send_response(
                                &mut stream,
                                &Response::Injected {
                                    session_id,
                                    bytes_written: len,
                                },
                            )
                            .await?;
                        }
                        Err(e) => {
                            send_response(
                                &mut stream,
                                &Response::Error {
                                    message: format!(
                                        "Write to session {session_id} failed: {e}"
                                    ),
                                },
                            )
                            .await?;
                        }
                    }
                }
                None => {
                    send_response(
                        &mut stream,
                        &Response::Error {
                            message: format!("Session {session_id} not found"),
                        },
                    )
                    .await?;
                }
            }
        }
        Request::Kill {
            session_id,
            quit_command,
            grace_seconds,
        } => {
            let session = {
                let st = state.lock().await;
                st.sessions.get(&session_id).cloned()
            };
            match session {
                Some(session) => {
                    if !session.is_running().await {
                        send_response(&mut stream, &Response::Killed { session_id })
                            .await?;
                        return Ok(());
                    }
                    let grace = grace_seconds.unwrap_or(5);
                    match session.kill(quit_command.as_deref(), grace).await {
                        Ok(()) => {
                            send_response(
                                &mut stream,
                                &Response::Killed { session_id },
                            )
                            .await?;
                        }
                        Err(e) => {
                            send_response(
                                &mut stream,
                                &Response::Error {
                                    message: format!(
                                        "Kill session {session_id} failed: {e}"
                                    ),
                                },
                            )
                            .await?;
                        }
                    }
                }
                None => {
                    send_response(
                        &mut stream,
                        &Response::Error {
                            message: format!("Session {session_id} not found"),
                        },
                    )
                    .await?;
                }
            }
        }
        _ => {
            send_response(
                &mut stream,
                &Response::Error {
                    message: "Unexpected request outside attach".into(),
                },
            )
            .await?;
        }
    }

    Ok(())
}

fn spawn_session_reaper(id: u32, session: Arc<PtySession>, state: Arc<Mutex<DaemonState>>) {
    tokio::spawn(async move {
        let _ = session.wait_exit_code().await;
        let mut st = state.lock().await;
        if st.sessions.remove(&id).is_some() {
            info!("Reaped session {id}");
        }
    });
}

/// Attach loop: subscribe to PTY output broadcast, forward client input to PTY.
async fn attach_loop(stream: UnixStream, session: Arc<PtySession>) -> Result<()> {
    // Trigger redraw: resize to current size sends SIGWINCH to child
    let (cols, rows) = session.get_size().await;
    let _ = session.resize(cols, rows).await;

    let (mut reader, mut writer) = stream.into_split();
    let session_r = session.clone();
    let session_id = session.id;

    // Subscribe to output broadcast
    let mut output_rx = session.subscribe_output();
    let drainer_done = session.drainer_done.clone();

    // Task: PTY output (via broadcast) → client
    let mut output_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = output_rx.recv() => {
                    match msg {
                        Ok(data) => {
                            let resp = Response::Output { data };
                            let json = serde_json::to_vec(&resp)
                                .expect("Response serialization is infallible");
                            let frame = protocol::encode(&json);
                            if writer.write_all(&frame).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            info!("Session {session_id} output: client lagged {n} messages");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
                _ = drainer_done.notified() => {
                    // Drainer exited (PTY EOF) — drain remaining messages
                    while let Ok(data) = output_rx.try_recv() {
                        let resp = Response::Output { data };
                        let json = serde_json::to_vec(&resp)
                            .expect("Response serialization is infallible");
                        let frame = protocol::encode(&json);
                        if writer.write_all(&frame).await.is_err() {
                            break;
                        }
                    }
                    break;
                }
            }
        }
        // Session exited
        let exit_code = session_r.wait_exit_code().await.ok();
        let resp = Response::SessionExited {
            session_id,
            exit_code,
        };
        let json =
            serde_json::to_vec(&resp).expect("Response serialization is infallible");
        let frame = protocol::encode(&json);
        let _ = writer.write_all(&frame).await;
    });

    // Task: client input → PTY
    let mut input_handle = tokio::spawn(async move {
        loop {
            let mut len_buf = [0u8; 4];
            if reader.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            if len > 1024 * 1024 {
                break;
            }
            let mut buf = vec![0u8; len];
            if reader.read_exact(&mut buf).await.is_err() {
                break;
            }
            let req: Request = match serde_json::from_slice(&buf) {
                Ok(r) => r,
                Err(_) => break,
            };
            match req {
                Request::Write { data } => {
                    if session.write_input(&data).await.is_err() {
                        break;
                    }
                }
                Request::Resize { cols, rows } => {
                    let _ = session.resize(cols, rows).await;
                }
                Request::Detach => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = &mut output_handle => { input_handle.abort(); },
        _ = &mut input_handle => { output_handle.abort(); },
    }

    Ok(())
}
