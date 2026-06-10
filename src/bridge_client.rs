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
use std::time::Duration;

use crate::framing;

/// Bounded connect timeout for the attached-mode bridge. A crashed / wedged
/// daemon whose port file lingers would otherwise hang `connect` (and the 2s
/// agent-roster sync loop that calls it) on the OS-default connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Bounded read/write timeout for the cookie/version/resize HANDSHAKE only — a
/// daemon that ACCEPTS the connection but never sends the version byte (wedged,
/// not refusing) would otherwise hang the blocking `read_exact`. Restored to
/// blocking before the streaming phase (the parked reader + input writer must
/// block). Kept under the 2s sync-loop period (with CONNECT_TIMEOUT) so a down
/// daemon can't stall the loop.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);

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
        // Bounded connect: a crashed/wedged daemon whose port file lingers must
        // not hang the attached-mode 2s sync loop on the OS-default connect.
        let mut stream = crate::ipc::connect_agent_timeout(home, name, CONNECT_TIMEOUT)
            .with_context(|| format!("connect to agent '{name}'"))?;
        // Bound the handshake I/O too: a daemon that ACCEPTS but never sends the
        // version byte (wedged, not refusing) would otherwise hang the blocking
        // `read_exact` below. Restored to blocking before the streaming phase.
        let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
        let _ = stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT));

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
        framing::write_resize(&mut stream, cols, rows).context("send initial resize")?;

        // Handshake done — restore BLOCKING for the streaming phase. The parked
        // reader (clone shares the socket) and the input writer must block, not
        // time out, for the lifetime of the pane.
        let _ = stream.set_read_timeout(None);
        let _ = stream.set_write_timeout(None);
        let reader = stream
            .try_clone()
            .context("clone bridge stream for read side")?;
        let writer = stream;

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

#[cfg(test)]
#[cfg(unix)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Instant;

    /// Regression: a daemon that ACCEPTS the connection but never sends the
    /// protocol-version byte (wedged, not refusing) must NOT hang
    /// `BridgeClient::connect` — the handshake read-timeout bounds it so the
    /// attached-mode 2s agent-roster sync loop can't stall during a daemon crash.
    /// Before the fix the blocking `read_exact` on the version byte hung forever.
    #[test]
    fn connect_against_wedged_daemon_is_bounded_not_hung() {
        let home = std::env::temp_dir().join(format!(
            "agend-bridge-timeout-{}-{}",
            std::process::id(),
            // a counter keeps parallel runs isolated
            {
                use std::sync::atomic::{AtomicU32, Ordering};
                static C: AtomicU32 = AtomicU32::new(0);
                C.fetch_add(1, Ordering::Relaxed)
            }
        ));
        let run = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&run).unwrap();
        // Fake an active daemon: .daemon (our live pid) + api.cookie so
        // `find_active_run_dir` + cookie-read succeed and we reach the handshake.
        crate::daemon::write_daemon_id(&run);
        crate::auth_cookie::issue(&run).expect("issue cookie");

        // A listener that ACCEPTS but never writes the version byte → the
        // handshake read would block forever without the timeout.
        let listener = TcpListener::bind((crate::ipc::LOOPBACK, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = std::thread::spawn(move || {
            // Hold the accepted connection open + silent for longer than the
            // handshake timeout so the client's read genuinely stalls.
            if let Ok((conn, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(5));
                drop(conn);
            }
        });
        crate::ipc::write_port(&run, "wedged-agent", port).unwrap();

        let start = Instant::now();
        let result = BridgeClient::connect(&home, "wedged-agent", 80, 24);
        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "a wedged daemon (accept-but-silent) must surface as a connect error, not a hang"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "connect must be bounded by CONNECT_TIMEOUT + HANDSHAKE_TIMEOUT (~2s), got {elapsed:?} — the sync loop must never hang"
        );

        drop(accept); // detached; the sleep finishes on its own
        std::fs::remove_dir_all(&home).ok();
    }
}
