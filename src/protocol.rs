use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Messages from client to daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    /// Spawn a new PTY session.
    Spawn {
        command: String,
        args: Vec<String>,
        cols: Option<u16>,
        rows: Option<u16>,
        env: Option<HashMap<String, String>>,
        ready_pattern: Option<String>,
        /// Instance name (for fleet-managed sessions).
        name: Option<String>,
    },
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
    /// Inject data into a session's PTY without attaching.
    Inject { session_id: u32, data: Vec<u8> },
    /// Kill a session.
    Kill {
        session_id: u32,
        quit_command: Option<String>,
        grace_seconds: Option<u32>,
    },

    // --- Fleet operations ---
    /// Start fleet from config.
    FleetStart {
        config_path: String,
        /// Only start these instances (empty = all).
        names: Vec<String>,
    },
    /// Stop fleet sessions.
    FleetStop {
        /// Only stop these instances (empty = all).
        names: Vec<String>,
    },
    /// Dynamically create a new instance.
    CreateInstance {
        name: String,
        command: String,
        args: Vec<String>,
        env: Option<HashMap<String, String>>,
        working_directory: Option<String>,
        topic_name: Option<String>,
        ready_pattern: Option<String>,
        cols: Option<u16>,
        rows: Option<u16>,
    },

    // --- Agent communication ---
    /// Agent replies to the user (routed via session_id).
    Reply {
        session_id: u32,
        text: String,
    },
    /// Agent sends a message to another instance.
    SendMessage {
        session_id: u32,
        target: String,
        text: String,
        kind: Option<String>,
        correlation_id: Option<String>,
    },
    /// Agent reads pending messages.
    Inbox {
        session_id: u32,
    },
}

/// Messages from daemon to client.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    Spawned { session_id: u32 },
    Attached { session_id: u32 },
    Sessions { sessions: Vec<SessionInfo> },
    Output { data: Vec<u8> },
    SessionExited {
        session_id: u32,
        exit_code: Option<i32>,
    },
    Error { message: String },
    Detached,
    Injected {
        session_id: u32,
        bytes_written: usize,
    },
    Killed { session_id: u32 },

    // --- Fleet responses ---
    FleetStarted { started: Vec<String> },
    FleetStopped { stopped: Vec<String> },

    /// Instance created dynamically.
    InstanceCreated {
        name: String,
        session_id: u32,
        topic_id: Option<i32>,
    },

    // --- Communication responses ---
    /// Reply/send acknowledged.
    Sent,
    /// Inbox messages.
    Messages { messages: Vec<InboxMessage> },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: u32,
    pub name: Option<String>,
    pub command: String,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub ready: bool,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub from: String,
    pub text: String,
    pub kind: Option<String>,
    pub correlation_id: Option<String>,
    pub timestamp: String,
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
