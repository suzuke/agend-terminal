//! Custom error types for daemon core paths.
//! Replaces anyhow on hot paths to avoid panic-on-unwrap taking down the daemon.

use std::fmt;

#[derive(Debug)]
#[allow(dead_code)]
pub enum AgendError {
    /// PTY spawn failed.
    SpawnFailed(String),
    /// Write to PTY failed.
    PtyWrite(std::io::Error),
    /// #3175: typed-inject readback-abort — the payload bytes WERE written to
    /// the PTY (they will render as an unsent draft once the pane settles) but
    /// rendering was never confirmed, so the submit key was withheld. Distinct
    /// from [`Self::PtyWrite`]: this is RECOVERABLE by resubmitting the submit
    /// key later (never by re-typing the payload). The delivery worker keys its
    /// requeue-resume on this variant.
    PtyWriteUnconfirmed(String),
    /// Socket connection failed.
    SocketConnect(std::io::Error),
    /// Agent not found in registry.
    AgentNotFound(String),
    /// API call failed.
    ApiError(String),
    /// Config parse error.
    ConfigError(String),
    /// Generic IO error.
    Io(std::io::Error),
}

impl fmt::Display for AgendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnFailed(msg) => write!(f, "spawn failed: {msg}"),
            Self::PtyWrite(e) => write!(f, "PTY write: {e}"),
            Self::PtyWriteUnconfirmed(msg) => write!(f, "PTY write unconfirmed: {msg}"),
            Self::SocketConnect(e) => write!(f, "socket connect: {e}"),
            Self::AgentNotFound(name) => write!(f, "agent '{name}' not found"),
            Self::ApiError(msg) => write!(f, "API: {msg}"),
            Self::ConfigError(msg) => write!(f, "config: {msg}"),
            Self::Io(e) => write!(f, "IO: {e}"),
        }
    }
}

impl std::error::Error for AgendError {}

impl From<std::io::Error> for AgendError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, AgendError>;
