use crate::protocol::{self, Request, Response, SessionInfo};
use crate::pty_session::PtySession;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{error, info};

struct DaemonState {
    sessions: HashMap<u32, Arc<PtySession>>,
    next_id: u32,
}

impl DaemonState {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            next_id: 1,
        }
    }
}

pub async fn run(socket_path: &Path) -> Result<()> {
    // Check if another daemon is alive before removing socket
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

    let listener = UnixListener::bind(socket_path).context("Failed to bind UDS")?;
    info!("Daemon listening on {:?}", socket_path);

    let state = Arc::new(Mutex::new(DaemonState::new()));

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
        } => {
            let cols = cols.unwrap_or(80);
            let rows = rows.unwrap_or(24);
            // Allocate ID under lock, spawn outside lock to avoid blocking
            let id = {
                let mut st = state.lock().await;
                let id = st.next_id;
                st.next_id += 1;
                id
            };
            let session = Arc::new(PtySession::spawn(id, &command, &args, cols, rows)?);
            {
                let mut st = state.lock().await;
                st.sessions.insert(id, session.clone());
            }
            // Spawn background reaper for this session
            spawn_session_reaper(id, session.clone(), state.clone());
            info!("Spawned session {id}: {command}");
            send_response(&mut stream, &Response::Spawned { session_id: id }).await?;
            attach_loop(stream, session, state.clone()).await?;
        }
        Request::Attach { session_id } => {
            let session = {
                let st = state.lock().await;
                st.sessions.get(&session_id).cloned()
            };
            match session {
                Some(session) => {
                    send_response(&mut stream, &Response::Attached { session_id }).await?;
                    attach_loop(stream, session, state.clone()).await?;
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
                sessions.push(SessionInfo {
                    id: s.id,
                    command: s.command.clone(),
                    running: s.is_running().await,
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
                    let len = data.len();
                    session.write_input(&data).await?;
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

/// Background task that waits for a session's child to exit, then removes it from state.
fn spawn_session_reaper(id: u32, session: Arc<PtySession>, state: Arc<Mutex<DaemonState>>) {
    tokio::spawn(async move {
        // Directly wait for child exit — works regardless of attach state
        let _ = session.wait_exit_code().await;
        let mut st = state.lock().await;
        if st.sessions.remove(&id).is_some() {
            info!("Reaped session {id}");
        }
    });
}

/// Main attach loop: bridge PTY output to client, client input to PTY.
async fn attach_loop(
    stream: UnixStream,
    session: Arc<PtySession>,
    _state: Arc<Mutex<DaemonState>>,
) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let session_r = session.clone();
    let session_id = session.id;

    // Task: PTY output → client
    let mut output_handle = tokio::spawn(async move {
        loop {
            match session_r.read_output().await {
                Ok(data) if data.is_empty() => break,
                Ok(data) => {
                    let resp = Response::Output { data };
                    let json = serde_json::to_vec(&resp)
                        .expect("Response serialization is infallible");
                    let frame = protocol::encode(&json);
                    if writer.write_all(&frame).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
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
