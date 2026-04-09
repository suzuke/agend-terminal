use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

pub struct PtySession {
    pub id: u32,
    pub command: String,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    reader: Arc<Mutex<Box<dyn Read + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    /// Cached exit status — once try_wait/wait returns, we store it here.
    exit_status: Arc<Mutex<Option<i32>>>,
    /// Notified when the child exits.
    pub exit_notify: Arc<Notify>,
}

impl PtySession {
    pub fn spawn(id: u32, command: &str, args: &[String], cols: u16, rows: u16) -> Result<Self> {
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

        let child = pair.slave.spawn_command(cmd).context("Failed to spawn")?;
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .context("Failed to clone PTY reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("Failed to take PTY writer")?;

        Ok(Self {
            id,
            command: command.to_string(),
            master: Arc::new(Mutex::new(pair.master)),
            reader: Arc::new(Mutex::new(reader)),
            writer: Arc::new(Mutex::new(writer)),
            child: Arc::new(Mutex::new(child)),
            exit_status: Arc::new(Mutex::new(None)),
            exit_notify: Arc::new(Notify::new()),
        })
    }

    /// Read output from PTY. Returns bytes read.
    pub async fn read_output(&self) -> Result<Vec<u8>> {
        let reader = self.reader.clone();
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; 4096];
            let mut r = reader.blocking_lock();
            let n = r.read(&mut buf).context("PTY read failed")?;
            buf.truncate(n);
            Ok(buf)
        })
        .await?
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
            .context("PTY resize failed")
    }

    /// Check if the child process is still running. Caches exit status.
    pub async fn is_running(&self) -> bool {
        // Check cache first
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

    /// Get cached exit code, or wait for child to exit.
    pub async fn wait_exit_code(&self) -> Result<i32> {
        // Return cached if available
        if let Some(code) = *self.exit_status.lock().await {
            return Ok(code);
        }
        let child = self.child.clone();
        let exit_status = self.exit_status.clone();
        let code = tokio::task::spawn_blocking(move || {
            let mut c = child.blocking_lock();
            let status = c.wait().context("Failed to wait for child")?;
            let code = status.exit_code() as i32;
            *exit_status.blocking_lock() = Some(code);
            Ok::<i32, anyhow::Error>(code)
        })
        .await??;
        self.exit_notify.notify_waiters();
        Ok(code)
    }
}
