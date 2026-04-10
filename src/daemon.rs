use crate::fleet::{ChannelConfig, FleetConfig};
use crate::instructions;
use crate::protocol::{self, InboxMessage, Request, Response, SessionInfo};
use crate::pty_session::PtySession;
use crate::telegram::TelegramChannel;
use anyhow::{Context, Result};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

const MAX_INBOX_SIZE: usize = 100;

pub struct DaemonState {
    pub sessions: HashMap<u32, Arc<PtySession>>,
    /// Instance name → session ID mapping.
    pub name_to_id: HashMap<String, u32>,
    pub next_id: u32,
    pub log_dir: PathBuf,
    /// Per-session message queue for notification+pull.
    pub inboxes: HashMap<u32, VecDeque<InboxMessage>>,
    /// Loaded fleet config (if any).
    pub fleet_config: Option<FleetConfig>,
    /// Telegram channel (if configured).
    pub telegram: Option<Arc<Mutex<TelegramChannel>>>,
}

impl DaemonState {
    fn new(log_dir: PathBuf) -> Self {
        Self {
            sessions: HashMap::new(),
            name_to_id: HashMap::new(),
            next_id: 1,
            log_dir,
            inboxes: HashMap::new(),
            fleet_config: None,
            telegram: None,
        }
    }

    pub fn find_session_by_name(&self, name: &str) -> Option<(u32, Arc<PtySession>)> {
        let id = self.name_to_id.get(name)?;
        let session = self.sessions.get(id)?;
        Some((*id, session.clone()))
    }

    fn find_name_by_id(&self, id: u32) -> Option<String> {
        self.name_to_id
            .iter()
            .find(|(_, &sid)| sid == id)
            .map(|(name, _)| name.clone())
    }

    pub fn enqueue_message(&mut self, session_id: u32, msg: InboxMessage) {
        let queue = self.inboxes.entry(session_id).or_default();
        if queue.len() >= MAX_INBOX_SIZE {
            queue.pop_front();
        }
        queue.push_back(msg);
    }

    fn drain_inbox(&mut self, session_id: u32) -> Vec<InboxMessage> {
        self.inboxes
            .get_mut(&session_id)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
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

    let mut sigterm = signal(SignalKind::terminate()).context("Failed to register SIGTERM")?;
    let mut sigint = signal(SignalKind::interrupt()).context("Failed to register SIGINT")?;

    let socket_path_owned = socket_path.to_path_buf();

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, state).await {
                        error!("Client handler error: {e:#}");
                    }
                });
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down...");
                graceful_shutdown(&state, &socket_path_owned).await;
                return Ok(());
            }
            _ = sigint.recv() => {
                info!("Received SIGINT, shutting down...");
                graceful_shutdown(&state, &socket_path_owned).await;
                return Ok(());
            }
        }
    }
}

async fn graceful_shutdown(state: &Arc<Mutex<DaemonState>>, socket_path: &Path) {
    let sessions: Vec<(u32, Arc<PtySession>)> = {
        let st = state.lock().await;
        st.sessions.iter().map(|(id, s)| (*id, s.clone())).collect()
    };

    if sessions.is_empty() {
        info!("No active sessions, exiting.");
    } else {
        info!("Killing {} session(s)...", sessions.len());
        for (id, session) in &sessions {
            if session.is_running().await {
                let mut child = session.child_lock().await;
                let _ = child.kill();
                info!("Sent SIGKILL to session {id}");
            }
        }

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mut all_exited = true;
            for (_, session) in &sessions {
                if session.is_running().await {
                    all_exited = false;
                    break;
                }
            }
            if all_exited {
                info!("All sessions exited.");
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                warn!("Grace period expired, forcing exit.");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    if let Err(e) = std::fs::remove_file(socket_path) {
        warn!("Failed to remove socket: {e}");
    } else {
        info!("Cleaned up socket: {}", socket_path.display());
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

/// Spawn a session and register it in state. Returns (id, session).
async fn spawn_session(
    state: &Arc<Mutex<DaemonState>>,
    name: Option<&str>,
    command: &str,
    args: &[String],
    cols: u16,
    rows: u16,
    env: Option<&HashMap<String, String>>,
    ready_pattern: Option<&str>,
    working_dir: Option<&Path>,
) -> Result<(u32, Arc<PtySession>)> {
    let (id, log_dir) = {
        let mut st = state.lock().await;
        let id = st.next_id;
        st.next_id += 1;
        (id, st.log_dir.clone())
    };
    let session = Arc::new(PtySession::spawn(
        id, name, command, args, cols, rows, env, ready_pattern, &log_dir, working_dir,
    )?);
    {
        let mut st = state.lock().await;
        st.sessions.insert(id, session.clone());
        if let Some(n) = name {
            st.name_to_id.insert(n.to_string(), id);
        }
        st.inboxes.insert(id, VecDeque::new());
    }
    spawn_session_reaper(id, session.clone(), state.clone());
    info!("Spawned session {id}{}: {command}",
        name.map(|n| format!(" ({n})")).unwrap_or_default());
    Ok((id, session))
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
            name,
        } => {
            let cols = cols.unwrap_or(80);
            let rows = rows.unwrap_or(24);
            let (id, session) = spawn_session(
                &state,
                name.as_deref(),
                &command,
                &args,
                cols,
                rows,
                env.as_ref(),
                ready_pattern.as_deref(),
                None,
            )
            .await?;
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
                    name: s.name.clone(),
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
                                    message: format!("Write failed: {e}"),
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
                        send_response(&mut stream, &Response::Killed { session_id }).await?;
                        return Ok(());
                    }
                    let grace = grace_seconds.unwrap_or(5);
                    match session.kill(quit_command.as_deref(), grace).await {
                        Ok(()) => {
                            send_response(&mut stream, &Response::Killed { session_id })
                                .await?;
                        }
                        Err(e) => {
                            send_response(
                                &mut stream,
                                &Response::Error {
                                    message: format!("Kill failed: {e}"),
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

        // --- Fleet operations ---
        Request::FleetStart { config_path, names } => {
            match FleetConfig::load(Path::new(&config_path)) {
                Ok(config) => {
                    let instance_names = if names.is_empty() {
                        config.instance_names()
                    } else {
                        names
                    };

                    let mut started = Vec::new();
                    {
                        let mut st = state.lock().await;
                        st.fleet_config = Some(config.clone());
                    }

                    for name in &instance_names {
                        if let Some(resolved) = config.resolve_instance(name) {
                            // Generate instructions before spawn
                            if let Some(ref dir) = resolved.working_directory {
                                instructions::generate(dir, &resolved.command);
                            }

                            match spawn_session(
                                &state,
                                Some(name),
                                &resolved.command,
                                &resolved.args,
                                resolved.cols.unwrap_or(120),
                                resolved.rows.unwrap_or(40),
                                Some(&resolved.env),
                                resolved.ready_pattern.as_deref(),
                                resolved.working_directory.as_deref(),
                            )
                            .await
                            {
                                Ok(_) => {
                                    started.push(name.clone());
                                }
                                Err(e) => {
                                    error!("Failed to start {name}: {e:#}");
                                }
                            }
                            // Staggered startup: 500ms between instances
                            if instance_names.len() > 1 {
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            }
                        } else {
                            warn!("Instance {name} not found in config");
                        }
                    }

                    // Set up Telegram channel if configured
                    if let Some(ChannelConfig::Telegram {
                        ref bot_token_env,
                        group_id,
                        ..
                    }) = config.channel
                    {
                        match std::env::var(bot_token_env) {
                            Ok(token) => {
                                // Build topic map from instances with existing topic_id
                                let topic_map: HashMap<String, i32> = config
                                    .instances
                                    .iter()
                                    .filter_map(|(name, inst)| {
                                        inst.topic_id.map(|tid| (name.clone(), tid))
                                    })
                                    .collect();

                                let channel = Arc::new(Mutex::new(TelegramChannel::new(
                                    &token, group_id, topic_map,
                                )));

                                // Auto-create topics for instances without topic_id
                                let mut config_updated = false;
                                for (name, inst) in config.instances.iter() {
                                    if inst.topic_id.is_none() && name != "general" {
                                        let mut ch = channel.lock().await;
                                        match ch.create_topic(name).await {
                                            Ok(tid) => {
                                                // Update config in memory for write-back
                                                // (actual write-back below)
                                                info!("Auto-created topic for {name}: {tid}");
                                                config_updated = true;
                                            }
                                            Err(e) => {
                                                warn!("Failed to create topic for {name}: {e:#}");
                                            }
                                        }
                                    }
                                    // General instance uses General topic (1)
                                    if name == "general" && inst.topic_id.is_none() {
                                        let mut ch = channel.lock().await;
                                        ch.register_topic("general", 1);
                                    }
                                }

                                // Write back updated topic_ids to fleet.yaml
                                if config_updated {
                                    let ch = channel.lock().await;
                                    if let Err(e) = write_back_topic_ids(
                                        Path::new(&config_path),
                                        &ch,
                                    ) {
                                        warn!("Failed to write back topic IDs: {e:#}");
                                    }
                                }

                                {
                                    let mut st = state.lock().await;
                                    st.telegram = Some(channel.clone());
                                }
                                TelegramChannel::start_polling(channel, state.clone());
                                info!("Telegram channel configured (group_id: {group_id})");
                            }
                            Err(_) => {
                                warn!(
                                    "Telegram bot token env '{bot_token_env}' not set, skipping"
                                );
                            }
                        }
                    }

                    send_response(&mut stream, &Response::FleetStarted { started }).await?;
                }
                Err(e) => {
                    send_response(
                        &mut stream,
                        &Response::Error {
                            message: format!("Failed to load config: {e}"),
                        },
                    )
                    .await?;
                }
            }
        }
        Request::FleetStop { names } => {
            let sessions_to_stop: Vec<(u32, String, Arc<PtySession>)> = {
                let st = state.lock().await;
                if names.is_empty() {
                    st.sessions
                        .iter()
                        .map(|(id, s)| {
                            let name = st.find_name_by_id(*id).unwrap_or_else(|| id.to_string());
                            (*id, name, s.clone())
                        })
                        .collect()
                } else {
                    names
                        .iter()
                        .filter_map(|n| {
                            st.find_session_by_name(n)
                                .map(|(id, s)| (id, n.clone(), s))
                        })
                        .collect()
                }
            };

            let mut stopped = Vec::new();
            for (_, name, session) in &sessions_to_stop {
                if session.is_running().await {
                    let _ = session.kill(None, 5).await;
                }
                stopped.push(name.clone());
            }

            // Stop Telegram polling if all sessions stopped
            if names.is_empty() {
                let telegram = {
                    let st = state.lock().await;
                    st.telegram.clone()
                };
                if let Some(tg) = telegram {
                    let ch = tg.lock().await;
                    ch.shutdown().await;
                    let mut st = state.lock().await;
                    st.telegram = None;
                }
            }

            send_response(&mut stream, &Response::FleetStopped { stopped }).await?;
        }

        // --- Agent communication ---
        Request::Reply { session_id, text } => {
            let sender_name = {
                let st = state.lock().await;
                st.find_name_by_id(session_id)
                    .unwrap_or_else(|| format!("session-{session_id}"))
            };
            info!("[reply from {sender_name}] {text}");

            // Route to Telegram if configured
            let telegram = {
                let st = state.lock().await;
                st.telegram.clone()
            };
            if let Some(tg) = telegram {
                let ch = tg.lock().await;
                match ch.send_to_topic(&sender_name, &text).await {
                    Ok(()) => {}
                    Err(e) => {
                        warn!("Telegram send failed: {e:#}");
                        send_response(
                            &mut stream,
                            &Response::Error {
                                message: format!("Telegram send failed: {e}"),
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }

            send_response(&mut stream, &Response::Sent).await?;
        }
        Request::SendMessage {
            session_id,
            target,
            text,
            kind,
            correlation_id,
        } => {
            let sender_name = {
                let st = state.lock().await;
                st.find_name_by_id(session_id)
                    .unwrap_or_else(|| format!("session-{session_id}"))
            };

            // Find target session
            let target_info = {
                let st = state.lock().await;
                st.find_session_by_name(&target)
            };

            match target_info {
                Some((target_id, target_session)) => {
                    // Enqueue message in target's inbox
                    let msg = InboxMessage {
                        from: sender_name.clone(),
                        text: text.clone(),
                        kind: kind.clone(),
                        correlation_id,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                    };
                    {
                        let mut st = state.lock().await;
                        st.enqueue_message(target_id, msg);
                    }

                    // Inject notification into target PTY
                    let display_text = if text.chars().count() > 200 {
                        let truncated: String = text.chars().take(200).collect();
                        format!("{truncated}... (Run: agend-terminal inbox)")
                    } else {
                        text.clone()
                    };
                    let submit = if target_session.command.contains("gemini") { "\n\r" } else { "\r" };
                    let notification = format!("\n[from:{sender_name}] {display_text}{submit}");
                    let _ = target_session.write_input(notification.as_bytes()).await;

                    info!("[{sender_name} → {target}] message delivered");
                    send_response(&mut stream, &Response::Sent).await?;
                }
                None => {
                    send_response(
                        &mut stream,
                        &Response::Error {
                            message: format!("Target instance '{target}' not found"),
                        },
                    )
                    .await?;
                }
            }
        }
        Request::Inbox { session_id } => {
            let messages = {
                let mut st = state.lock().await;
                st.drain_inbox(session_id)
            };
            send_response(&mut stream, &Response::Messages { messages }).await?;
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

/// Write auto-created topic IDs back to fleet.yaml.
fn write_back_topic_ids(config_path: &Path, channel: &TelegramChannel) -> Result<()> {
    let content = std::fs::read_to_string(config_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;

    if let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) {
        for (name, topic_id) in channel.get_topic_map() {
            let key = serde_yaml::Value::String(name.clone());
            if let Some(inst) = instances.get_mut(&key).and_then(|v| v.as_mapping_mut()) {
                let tid_key = serde_yaml::Value::String("topic_id".to_string());
                if !inst.contains_key(&tid_key) {
                    inst.insert(
                        tid_key,
                        serde_yaml::Value::Number(serde_yaml::Number::from(*topic_id)),
                    );
                }
            }
        }
    }

    let yaml = serde_yaml::to_string(&doc)?;
    std::fs::write(config_path, yaml)?;
    info!("Updated fleet.yaml with topic IDs");
    Ok(())
}

fn spawn_session_reaper(id: u32, session: Arc<PtySession>, state: Arc<Mutex<DaemonState>>) {
    tokio::spawn(async move {
        let _ = session.wait_exit_code().await;
        let mut st = state.lock().await;
        if st.sessions.remove(&id).is_some() {
            // Also clean up name mapping
            st.name_to_id.retain(|_, sid| *sid != id);
            st.inboxes.remove(&id);
            info!("Reaped session {id}");
        }
    });
}

async fn attach_loop(stream: UnixStream, session: Arc<PtySession>) -> Result<()> {
    // Resize to trigger SIGWINCH for redraw
    let (cols, rows) = session.get_size().await;
    let _ = session.resize(cols, rows).await;

    let (mut reader, mut writer) = stream.into_split();
    let session_r = session.clone();
    let session_id = session.id;

    // Atomically subscribe + dump to avoid losing output between the two
    let (mut output_rx, screen_dump) = session.subscribe_with_dump();
    if !screen_dump.is_empty() {
        let resp = Response::Output { data: screen_dump };
        let json = serde_json::to_vec(&resp).expect("Response serialization is infallible");
        let frame = protocol::encode(&json);
        let _ = writer.write_all(&frame).await;
    }

    let drainer_done = session.drainer_done.clone();

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
        let exit_code = session_r.wait_exit_code().await.ok();
        let resp = Response::SessionExited {
            session_id,
            exit_code,
        };
        let json = serde_json::to_vec(&resp).expect("Response serialization is infallible");
        let frame = protocol::encode(&json);
        let _ = writer.write_all(&frame).await;
    });

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
