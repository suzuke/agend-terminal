use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct PtySession {
    pub id: u32,
    pub command: String,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    reader: Arc<Mutex<Box<dyn Read + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
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
        })
    }

    /// Read output from PTY. Returns data in a Vec to avoid lifetime issues.
    pub async fn read_output(&self, buf: &mut Vec<u8>) -> Result<usize> {
        let reader = self.reader.clone();
        let mut tmp = vec![0u8; buf.len()];
        let result = tokio::task::spawn_blocking(move || {
            let mut r = reader.blocking_lock();
            let n = r.read(&mut tmp).context("PTY read failed")?;
            Ok::<(Vec<u8>, usize), anyhow::Error>((tmp, n))
        })
        .await??;
        let (tmp, n) = result;
        buf[..n].copy_from_slice(&tmp[..n]);
        Ok(n)
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

    /// Check if the child process is still running.
    pub async fn is_running(&self) -> bool {
        let mut child = self.child.lock().await;
        match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        }
    }

    /// Wait for the child to exit and return exit code.
    pub async fn wait(&self) -> Result<Option<i32>> {
        let child = self.child.clone();
        tokio::task::spawn_blocking(move || {
            let mut c = child.blocking_lock();
            let status = c.wait().context("Failed to wait for child")?;
            // portable_pty ExitStatus::exit_code() returns u32
            Ok(Some(status.exit_code() as i32))
        })
        .await?
    }
}
