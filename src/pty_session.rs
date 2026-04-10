use crate::vterm::VTerm;
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
    pub name: Option<String>,
    pub command: String,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    exit_status: Arc<Mutex<Option<i32>>>,
    size: Arc<Mutex<(u16, u16)>>,
    ready: Arc<AtomicBool>,
    output_tx: broadcast::Sender<Vec<u8>>,
    pub drainer_done: Arc<Notify>,
    /// Virtual terminal for screen state tracking.
    vterm: Arc<std::sync::Mutex<VTerm>>,
    #[allow(dead_code)]
    child_pid: Option<u32>,
}

impl PtySession {
    pub fn spawn(
        id: u32,
        name: Option<&str>,
        command: &str,
        args: &[String],
        cols: u16,
        rows: u16,
        env: Option<&HashMap<String, String>>,
        ready_pattern: Option<&str>,
        log_dir: &std::path::Path,
        working_dir: Option<&std::path::Path>,
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

        // Set working directory
        if let Some(dir) = working_dir {
            cmd.cwd(dir);
        }

        // Base env
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("FORCE_COLOR", "1");
        if std::env::var("LANG").is_err() {
            cmd.env("LANG", "en_US.UTF-8");
        }

        // User env (from config) — set before system env so it can't override
        if let Some(env_map) = env {
            for (k, v) in env_map {
                cmd.env(k, v);
            }
        }

        // System env — set last, cannot be overridden by user config
        cmd.env("AGEND_SESSION_ID", id.to_string());
        if let Some(n) = name {
            cmd.env("AGEND_INSTANCE_NAME", n);
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
        let vterm = Arc::new(std::sync::Mutex::new(VTerm::new(cols, rows)));

        // Spawn background output drainer — owns a clone of output_tx
        Self::spawn_output_drainer(
            reader,
            log_file,
            output_tx.clone(),
            ready_regex,
            ready.clone(),
            id,
            drainer_done.clone(),
            vterm.clone(),
        );

        Ok(Self {
            id,
            name: name.map(|s| s.to_string()),
            command: command.to_string(),
            master: Arc::new(Mutex::new(pair.master)),
            writer: Arc::new(Mutex::new(writer)),
            child: Arc::new(Mutex::new(child)),
            exit_status: Arc::new(Mutex::new(None)),
            size: Arc::new(Mutex::new((cols, rows))),
            ready,
            vterm,
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
        vterm: Arc<std::sync::Mutex<VTerm>>,
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

                // Feed into virtual terminal for screen state tracking
                if let Ok(mut vt) = vterm.lock() {
                    vt.process(data);
                }

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
            drainer_done.notify_waiters();
        });
    }

    /// Subscribe to PTY output broadcast.
    #[allow(dead_code)]
    pub fn subscribe_output(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    /// Atomically subscribe and dump screen — no output lost between dump and subscribe.
    /// Holds vterm lock during both operations so drainer can't send output in between.
    pub fn subscribe_with_dump(&self) -> (broadcast::Receiver<Vec<u8>>, Vec<u8>) {
        let vt = self.vterm.lock().unwrap_or_else(|e| {
            tracing::warn!("VTerm poisoned: {e}");
            e.into_inner()
        });
        let rx = self.output_tx.subscribe();
        let dump = vt.dump_screen();
        (rx, dump)
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

    /// Resize the PTY and virtual terminal.
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
        if let Ok(mut vt) = self.vterm.lock() {
            vt.resize(cols, rows);
        }
        Ok(())
    }

    /// Dump the current virtual terminal screen as ANSI escape sequences.
    #[allow(dead_code)]
    pub fn dump_screen(&self) -> Vec<u8> {
        self.vterm
            .lock()
            .map(|vt| vt.dump_screen())
            .unwrap_or_else(|e| {
                tracing::warn!("VTerm poisoned: {e}");
                Vec::new()
            })
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
