//! Supervisor IPC protocol (NDJSON over Unix domain socket).
//!
//! One JSON document per line. Client opens a fresh connection per request,
//! sends exactly one [`Request`], reads back exactly one [`Response`], closes.
//! Long-running operations (upgrade) keep the socket open and stream
//! progress responses; the final response has `final: true`.
//!
//! The supervisor must never break wire compat here lightly: an old daemon
//! binary might be the one reading the socket after a rolled-back upgrade,
//! and we want that to still work.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::time::Duration;

/// Wire version — bump only for breaking protocol changes. Responses
/// include `version` so clients can detect an incompatible supervisor.
pub const WIRE_VERSION: u32 = 1;

/// Requests sent by CLI/daemon → supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Sanity check: round-trip with timing info, version.
    Ping,

    /// Fetch current supervisor state: daemon pid, running since,
    /// last upgrade outcome.
    Status,

    /// Trigger a daemon upgrade. Caller has already staged the new binary
    /// and populated the `current` symlink — supervisor's job is to stop
    /// the old daemon, start the new one, verify stability, and roll back
    /// on failure.
    Upgrade(UpgradeArgs),

    /// Daemon-side handshake. Sent by a freshly-started daemon to signal
    /// "I booted successfully; stability clock starts now." Supervisor
    /// uses this to finalize an in-flight upgrade.
    Ready { pid: u32, version: String },

    /// Daemon-side graceful-shutdown notice (optional; supervisor also
    /// detects via child exit). Lets the supervisor skip the respawn
    /// loop when the daemon exited intentionally.
    ShuttingDown { reason: String },
}

/// Parameters for [`Request::Upgrade`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeArgs {
    /// Hex-encoded sha256 of the new binary (must already live at
    /// `$AGEND_HOME/bin/store/<hash>`; supervisor only derefs the path).
    pub new_hash: String,
    /// Hex-encoded sha256 of the previous binary, for rollback. Also
    /// staged under `bin/store/`.
    pub prev_hash: String,
    /// Optional human-visible version string, shown in logs and injected
    /// into agent system messages.
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    /// Stability window — supervisor waits this long after launching the
    /// new daemon and counts crashes. 0 disables the check.
    #[serde(default = "default_stability_secs")]
    pub stability_secs: u64,
    /// How long to wait for the new daemon's `Ready` ping before giving
    /// up and rolling back. 0 disables the check (rare; mostly for
    /// self-test-only upgrades).
    #[serde(default = "default_ready_timeout_secs")]
    pub ready_timeout_secs: u64,
}

fn default_stability_secs() -> u64 {
    60
}
fn default_ready_timeout_secs() -> u64 {
    60
}

/// Responses sent supervisor → caller. Multiple `progress` responses may
/// precede the single `final: true` response for long-running ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Generic success. `final: true` terminates the stream.
    Ok {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        data: Option<serde_json::Value>,
        #[serde(default = "default_true")]
        r#final: bool,
        version: u32,
    },
    /// Generic error. Always terminal.
    Err {
        error: String,
        version: u32,
    },
    /// Progress update during a long-running upgrade. Not terminal.
    Progress {
        stage: UpgradeStage,
        message: String,
        version: u32,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeStage {
    /// Supervisor received request and began staging.
    Accepted,
    /// Running `new_binary --self-test` (invoked with AGEND_SELF_TEST=1).
    SelfTesting,
    /// Stopping the old daemon (SIGTERM + grace).
    StoppingDaemon,
    /// Spawning the new daemon under the supervisor.
    StartingDaemon,
    /// Waiting for the new daemon's `Ready` ping.
    WaitingReady,
    /// In the post-ready stability window — watching for repeat crashes.
    Stabilising,
    /// Upgrade completed successfully.
    Succeeded,
    /// Upgrade failed; rolling back to the previous binary.
    RollingBack,
    /// Rollback completed (daemon running on prev binary).
    RolledBack,
}

/// Build a terminal [`Response::Ok`] carrying the default version stamp.
pub fn ok_final(message: Option<String>, data: Option<serde_json::Value>) -> Response {
    Response::Ok {
        message,
        data,
        r#final: true,
        version: WIRE_VERSION,
    }
}

/// Build a non-terminal [`Response::Progress`].
pub fn progress(stage: UpgradeStage, message: impl Into<String>) -> Response {
    Response::Progress {
        stage,
        message: message.into(),
        version: WIRE_VERSION,
    }
}

/// Build a terminal [`Response::Err`].
pub fn err(message: impl Into<String>) -> Response {
    Response::Err {
        error: message.into(),
        version: WIRE_VERSION,
    }
}

// --- I/O helpers -----------------------------------------------------------

/// Read one NDJSON message of type `T` from `reader`. Returns `Ok(None)` if
/// the peer closed before sending anything; `Err` on malformed JSON.
pub fn read_one<T: for<'de> Deserialize<'de>, R: Read>(
    reader: &mut BufReader<R>,
) -> std::io::Result<Option<T>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    // Strip trailing newline (read_line leaves it). Tolerate \r\n.
    while matches!(line.as_bytes().last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    if line.is_empty() {
        return Ok(None);
    }
    serde_json::from_str::<T>(&line)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write one NDJSON message of type `T` to `writer`. Appends `\n`.
pub fn write_one<T: Serialize, W: Write>(writer: &mut W, value: &T) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    buf.push(b'\n');
    writer.write_all(&buf)?;
    writer.flush()
}

// --- Unix-only connection helpers ------------------------------------------
//
// The supervisor is Unix-only. Windows code paths must guard any call into
// these helpers; we keep the types above portable so the protocol module can
// still be type-checked on Windows builds.

#[cfg(unix)]
pub mod uds {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// Default per-op timeout for client sockets. Generous enough for a
    /// full upgrade (self-test + stop + start + stability window can
    /// easily take 60s) but tight enough to avoid indefinite hangs.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

    /// Open a UDS connection to the supervisor socket with read/write
    /// timeouts applied.
    pub fn connect(socket: &Path) -> std::io::Result<UnixStream> {
        let stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(DEFAULT_TIMEOUT))?;
        stream.set_write_timeout(Some(DEFAULT_TIMEOUT))?;
        Ok(stream)
    }

    /// Bind the supervisor's listening socket. Removes any stale socket
    /// file first (supervisor is a singleton — a leftover file from a
    /// crash should not block startup). Sets permissions to 0o600 so
    /// only the owning user can connect.
    pub fn bind(socket: &Path) -> std::io::Result<UnixListener> {
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Best-effort clean: ignore NotFound; propagate other errors.
        match std::fs::remove_file(socket) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(socket)?;
        // Restrict to owner. 0o600 is enough because the socket lives
        // inside $AGEND_HOME (already user-scoped), but we're belt-
        // and-braces here.
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(socket, perms)?;
        Ok(listener)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_args_defaults() {
        let args: UpgradeArgs = serde_json::from_str(
            r#"{"new_hash":"a","prev_hash":"b"}"#,
        )
        .expect("parse");
        assert_eq!(args.stability_secs, 60);
        assert_eq!(args.ready_timeout_secs, 60);
    }

    #[test]
    fn request_roundtrip() {
        let r = Request::Upgrade(UpgradeArgs {
            new_hash: "a".into(),
            prev_hash: "b".into(),
            from_version: Some("0.3.0".into()),
            to_version: Some("0.4.0".into()),
            stability_secs: 30,
            ready_timeout_secs: 30,
        });
        let json = serde_json::to_string(&r).expect("ser");
        let back: Request = serde_json::from_str(&json).expect("de");
        match back {
            Request::Upgrade(a) => {
                assert_eq!(a.new_hash, "a");
                assert_eq!(a.stability_secs, 30);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn ready_request_roundtrip() {
        let r = Request::Ready {
            pid: 1234,
            version: "0.4.0".into(),
        };
        let json = serde_json::to_string(&r).expect("ser");
        assert!(json.contains("\"op\":\"ready\""));
        let back: Request = serde_json::from_str(&json).expect("de");
        match back {
            Request::Ready { pid, version } => {
                assert_eq!(pid, 1234);
                assert_eq!(version, "0.4.0");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn progress_not_final() {
        let p = progress(UpgradeStage::SelfTesting, "running smoke tests");
        let json = serde_json::to_string(&p).expect("ser");
        assert!(json.contains("\"stage\":\"self_testing\""));
    }

    #[test]
    fn ndjson_framing() {
        use std::io::Cursor;
        let req = Request::Ping;
        let mut buf = Vec::new();
        write_one(&mut buf, &req).expect("write");
        assert_eq!(*buf.last().expect("nonempty"), b'\n');
        let mut reader = BufReader::new(Cursor::new(buf));
        let back: Request = read_one(&mut reader)
            .expect("read")
            .expect("non-empty");
        assert!(matches!(back, Request::Ping));
    }

    #[test]
    fn reject_malformed() {
        use std::io::Cursor;
        let buf = b"not json\n".to_vec();
        let mut reader = BufReader::new(Cursor::new(buf));
        let res: std::io::Result<Option<Request>> = read_one(&mut reader);
        assert!(res.is_err());
    }
}
