//! Reusable client for the daemon's per-agent TUI bridge.
//!
//! Carries the connect + cookie + protocol-version handshake and the framed
//! send side (input, resize). Leaves the read side to the caller so each
//! consumer can decide how to render incoming frames:
//!
//! - `tui::attach` pipes them to `stdout` (raw mode CLI).
//! - `app`'s future `Pane::Remote` will feed them into a vterm instance
//!   alongside local panes.
//!
//! Network layout mirrors the daemon side (`daemon::tui_bridge`): the first
//! byte sent is a 32-byte API cookie (see `auth_cookie`), the first byte
//! received is [`framing::PROTOCOL_VERSION`]. Anything else is framed
//! payload consumed via [`framing::read_tagged_frame`].

use anyhow::{Context, Result};
use std::net::TcpStream;
use std::path::Path;

use crate::framing;

/// Open + authenticated TCP bridge to one daemon-hosted agent.
///
/// `reader` is intentionally exposed as an owned [`TcpStream`] so the caller
/// can move it into a thread (stdout pump, vterm feeder, …) without an
/// extra layer of synchronization. `writer` stays on the [`BridgeClient`]
/// for [`send_input`] / [`send_resize`].
///
/// [`send_input`]: Self::send_input
/// [`send_resize`]: Self::send_resize
pub struct BridgeClient {
    writer: TcpStream,
    reader: Option<TcpStream>,
}

impl BridgeClient {
    /// Connect to `name` on the local daemon, send the cookie, verify the
    /// protocol version, and send the initial resize frame.
    ///
    /// Errors carry enough context to tell `name`-unknown from
    /// cookie-rejected from version-mismatch apart without the caller having
    /// to peek at the wire.
    pub fn connect(home: &Path, name: &str, cols: u16, rows: u16) -> Result<Self> {
        let mut stream = crate::ipc::connect_agent(home, name)
            .with_context(|| format!("connect to agent '{name}'"))?;

        let run = crate::daemon::find_active_run_dir(home)
            .context("no active daemon (run dir not found)")?;
        let cookie = crate::auth_cookie::read_cookie(&run).context("read api.cookie")?;
        crate::auth_cookie::write_tui_auth(&mut stream, &cookie).context("send TUI auth cookie")?;

        let mut version_buf = [0u8; 1];
        use std::io::Read;
        stream
            .read_exact(&mut version_buf)
            .context("read protocol version")?;
        if version_buf[0] != framing::PROTOCOL_VERSION {
            anyhow::bail!(
                "protocol version mismatch: server={} client={}",
                version_buf[0],
                framing::PROTOCOL_VERSION
            );
        }

        let reader = stream
            .try_clone()
            .context("clone bridge stream for read side")?;
        let mut writer = stream;
        framing::write_resize(&mut writer, cols, rows).context("send initial resize")?;

        Ok(Self {
            writer,
            reader: Some(reader),
        })
    }

    /// Take the read side so the caller can park a thread on it. Subsequent
    /// calls return `None` — a single bridge has a single reader.
    pub fn take_reader(&mut self) -> Option<TcpStream> {
        self.reader.take()
    }

    /// Send a framed data payload (keystrokes, paste text, …) to the agent.
    pub fn send_input(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        framing::write_frame(&mut self.writer, bytes)
    }

    /// Send a framed resize event to the agent.
    pub fn send_resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        framing::write_resize(&mut self.writer, cols, rows)
    }
}
