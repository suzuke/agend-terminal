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
    if socket_path.exists() {
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
        Request::Spawn { command, args } => {
            let (session_id, session) = {
                let mut st = state.lock().await;
                let id = st.next_id;
                st.next_id += 1;
                let session = Arc::new(PtySession::spawn(id, &command, &args, 80, 24)?);
                st.sessions.insert(id, session.clone());
                (id, session)
            };
            info!("Spawned session {session_id}: {command}");
            send_response(&mut stream, &Response::Spawned { session_id }).await?;
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

/// Main attach loop: bridge PTY output to client, client input to PTY.
async fn attach_loop(
    stream: UnixStream,
    session: Arc<PtySession>,
    state: Arc<Mutex<DaemonState>>,
) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let session_r = session.clone();
    let session_id = session.id;

    // Task: PTY output → client
    let output_handle = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            match session_r.read_output(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let resp = Response::Output {
                        data: buf[..n].to_vec(),
                    };
                    let json = serde_json::to_vec(&resp).unwrap();
                    let frame = protocol::encode(&json);
                    if writer.write_all(&frame).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // Session exited
        let resp = Response::SessionExited {
            session_id,
            exit_code: session_r.wait().await.ok().flatten(),
        };
        let json = serde_json::to_vec(&resp).unwrap();
        let frame = protocol::encode(&json);
        let _ = writer.write_all(&frame).await;
    });

    // Task: client input → PTY
    let input_handle = tokio::spawn(async move {
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
        _ = output_handle => {},
        _ = input_handle => {},
    }

    // Clean up dead sessions
    {
        let mut st = state.lock().await;
        if let Some(s) = st.sessions.get(&session_id) {
            if !s.is_running().await {
                st.sessions.remove(&session_id);
                info!("Cleaned up session {session_id}");
            }
        }
    }

    Ok(())
}
