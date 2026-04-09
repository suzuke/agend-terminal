use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use regex::Regex;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, Notify};
use tracing::info;

pub struct PtySession {
    pub id: u32,
    pub command: String,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    exit_status: Arc<Mutex<Option<i32>>>,
    /// Current terminal size.
    size: Arc<Mutex<(u16, u16)>>,
    /// Whether the CLI is ready (matched ready_pattern).
    ready: Arc<AtomicBool>,
    /// Broadcast channel for PTY output — attach clients subscribe here.
    output_tx: broadcast::Sender<Vec<u8>>,
    /// Notified when the output drainer exits (PTY EOF).
    pub drainer_done: Arc<Notify>,
    /// Process ID for signaling.
    #[allow(dead_code)]
    child_pid: Option<u32>,
}

impl PtySession {
    pub fn spawn(
        id: u32,
        command: &str,
        args: &[String],
        cols: u16,
        rows: u16,
        env: Option<&HashMap<String, String>>,
        ready_pattern: Option<&str>,
        log_dir: &std::path::Path,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to open PTY")?;

        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);

        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("FORCE_COLOR", "1");
        if std::env::var("LANG").is_err() {
            cmd.env("LANG", "en_US.UTF-8");
        }

        if let Some(env_map) = env {
            for (k, v) in env_map {
                cmd.env(k, v);
            }
        }

        let child = pair.slave.spawn_command(cmd).context("Failed to spawn")?;
        let child_pid = child.process_id();
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .context("Failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("Failed to take PTY writer")?;

        // Set up output capture log
        let session_dir = log_dir.join(id.to_string());
        std::fs::create_dir_all(&session_dir)?;
        let log_path = session_dir.join("output.log");
        let log_file =
            std::fs::File::create(&log_path).context("Failed to create output log")?;
        info!("Output capture: {}", log_path.display());

        let ready_regex = ready_pattern
            .map(|p| Regex::new(p).context("Invalid ready_pattern regex"))
            .transpose()?;

        // Broadcast channel: 64 buffered messages
        let (output_tx, _) = broadcast::channel(64);

        let ready = Arc::new(AtomicBool::new(ready_pattern.is_none()));

        let drainer_done = Arc::new(Notify::new());

        // Spawn background output drainer — owns a clone of output_tx
        Self::spawn_output_drainer(
            reader,
            log_file,
            output_tx.clone(),
            ready_regex,
            ready.clone(),
            id,
            drainer_done.clone(),
        );

        Ok(Self {
            id,
            command: command.to_string(),
            master: Arc::new(Mutex::new(pair.master)),
            writer: Arc::new(Mutex::new(writer)),
            child: Arc::new(Mutex::new(child)),
            exit_status: Arc::new(Mutex::new(None)),
            size: Arc::new(Mutex::new((cols, rows))),
            ready,
            output_tx,
            drainer_done,
            child_pid,
        })
    }

    /// Background task: reads PTY output, writes to log, broadcasts to subscribers.
    fn spawn_output_drainer(
        mut reader: Box<dyn Read + Send>,
        mut log_file: std::fs::File,
        tx: broadcast::Sender<Vec<u8>>,
        ready_regex: Option<Regex>,
        ready: Arc<AtomicBool>,
        session_id: u32,
        drainer_done: Arc<Notify>,
    ) {
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; 4096];
            let mut ready_buf = String::new();
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                let data = &buf[..n];

                let _ = log_file.write_all(data);
                let _ = log_file.flush();

                // Check ready pattern
                if !ready.load(Ordering::Relaxed) {
                    if let Some(ref re) = ready_regex {
                        ready_buf.push_str(&String::from_utf8_lossy(data));
                        if ready_buf.len() > 8192 {
                            let mut start = ready_buf.len() - 8192;
                            while !ready_buf.is_char_boundary(start) {
                                start += 1;
                            }
                            ready_buf = ready_buf[start..].to_string();
                        }
                        if re.is_match(&ready_buf) {
                            ready.store(true, Ordering::Relaxed);
                            info!("Session {session_id} ready (matched pattern)");
                        }
                    }
                }

                let _ = tx.send(data.to_vec());
            }
            // Signal that drainer has exited (PTY EOF)
            drainer_done.notify_waiters();
        });
    }

    /// Subscribe to PTY output broadcast.
    pub fn subscribe_output(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    /// Write input to PTY master fd. This is the atomic write path.
    pub async fn write_input(&self, data: &[u8]) -> Result<()> {
        let writer = self.writer.clone();
        let data = data.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut w = writer.blocking_lock();
            w.write_all(&data).context("PTY write failed")?;
            w.flush().context("PTY flush failed")
        })
        .await?
    }

    /// Resize the PTY.
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let master = self.master.lock().await;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("PTY resize failed")?;
        *self.size.lock().await = (cols, rows);
        Ok(())
    }

    pub async fn get_size(&self) -> (u16, u16) {
        *self.size.lock().await
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    pub async fn is_running(&self) -> bool {
        if self.exit_status.lock().await.is_some() {
            return false;
        }
        let mut child = self.child.lock().await;
        match child.try_wait() {
            Ok(Some(status)) => {
                *self.exit_status.lock().await = Some(status.exit_code() as i32);
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Access the child lock (for daemon shutdown).
    pub async fn child_lock(&self) -> tokio::sync::MutexGuard<'_, Box<dyn portable_pty::Child + Send>> {
        self.child.lock().await
    }

    pub async fn get_exit_code(&self) -> Option<i32> {
        *self.exit_status.lock().await
    }

    pub async fn wait_exit_code(&self) -> Result<i32> {
        loop {
            if let Some(code) = *self.exit_status.lock().await {
                return Ok(code);
            }
            {
                let mut child = self.child.lock().await;
                if let Ok(Some(status)) = child.try_wait() {
                    let code = status.exit_code() as i32;
                    *self.exit_status.lock().await = Some(code);
                    return Ok(code);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Kill the child process. Optionally inject a quit command first.
    pub async fn kill(&self, quit_command: Option<&str>, grace_seconds: u32) -> Result<()> {
        if let Some(cmd) = quit_command {
            let mut data = cmd.as_bytes().to_vec();
            data.push(b'\r');
            let _ = self.write_input(&data).await;
            for _ in 0..(grace_seconds * 10) {
                if !self.is_running().await {
                    info!("Session {} exited gracefully", self.id);
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }

        {
            let mut child = self.child.lock().await;
            child.kill().context("Failed to kill child")?;
        }
        info!("Session {} killed", self.id);

        // Brief poll for exit status
        for _ in 0..20 {
            if !self.is_running().await {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Ok(())
    }
}
