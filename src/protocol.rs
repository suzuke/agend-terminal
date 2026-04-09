use serde::{Deserialize, Serialize};

/// Messages from client to daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    /// Spawn a new PTY session.
    Spawn { command: String, args: Vec<String> },
    /// Attach to an existing session.
    Attach { session_id: u32 },
    /// List all sessions.
    List,
    /// Write data to a session's PTY master fd.
    Write { data: Vec<u8> },
    /// Resize the PTY.
    Resize { cols: u16, rows: u16 },
    /// Detach from current session.
    Detach,
}

/// Messages from daemon to client.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    /// Session spawned successfully.
    Spawned { session_id: u32 },
    /// Attached to session.
    Attached { session_id: u32 },
    /// Session list.
    Sessions { sessions: Vec<SessionInfo> },
    /// PTY output data.
    Output { data: Vec<u8> },
    /// Session exited.
    SessionExited { session_id: u32, exit_code: Option<i32> },
    /// Error.
    Error { message: String },
    /// Detached confirmation.
    Detached,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u32,
    pub command: String,
    pub running: bool,
}

/// Frame protocol: 4-byte big-endian length prefix + JSON payload.
pub fn encode(msg: &[u8]) -> Vec<u8> {
    assert!(msg.len() <= u32::MAX as usize, "Frame payload exceeds 4GB");
    let len = msg.len() as u32;
    let mut frame = Vec::with_capacity(4 + msg.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(msg);
    frame
}
