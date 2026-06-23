//! #2413 Shadow Observer — LOCAL plane (claude lifecycle hooks).
//!
//! A spawned claude's hooks emit one JSON frame per lifecycle event to a per-daemon
//! unix socket, carrying a per-session 256-bit token. This module:
//! 1. mints the token + resolves the socket path ([`new_session_token`],
//!    [`socket_path`]) — injected into the single spawned claude's env at spawn time
//!    (`AGENT_TERMINAL_SOCKET` / `AGENT_TERMINAL_SESSION`), scoped to that process, so
//!    `~/.claude` global is never touched;
//! 2. validates the token against a per-session registry ([`register`], [`resolve`])
//!    so another LOCAL process cannot forge evidence for an agent;
//! 3. maps the hook event → [`Evidence`] and pushes it into a per-agent buffer
//!    ([`push`], [`drain`], [`peek`]) that a later reducer (Phase B, OUT OF SCOPE)
//!    consumes;
//! 4. runs the socket event server ([`start`]).
//!
//! Spike discipline: prove hook→evidence under the native TUI + no global touch. No
//! reducer. `flag default OFF` via `AGEND_SHADOW_OBSERVER=1`.

pub mod evidence;
pub mod reducer;

use evidence::{evidence_kind_for_hook, Evidence};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Per-agent evidence buffer cap — drop-oldest beyond this. A spike bound; the
/// reducer drains far faster than hooks fire, so this only guards a never-drained
/// agent from unbounded growth.
const BUFFER_CAP: usize = 256;

/// One wire frame the hook emit writes to the socket (a single JSON line). Mirrors
/// the existing `hook-event` payload fields + the session token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowFrame {
    /// Per-session 256-bit token (hex) — proves the frame came from THIS spawned
    /// claude, not another local process.
    pub token: String,
    pub hook_event_name: String,
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
}

/// `true` when the Shadow Observer local plane is enabled (`AGEND_SHADOW_OBSERVER=1`).
/// Default OFF — zero behavior change for every existing spawn.
pub fn enabled() -> bool {
    std::env::var("AGEND_SHADOW_OBSERVER").as_deref() == Ok("1")
}

/// The per-daemon unix socket the hooks emit to. One socket for the daemon; the
/// token attributes each frame to its agent.
pub fn socket_path(home: &Path) -> PathBuf {
    home.join("shadow-events.sock")
}

/// Mint a fresh 256-bit session token (64 hex chars). Cryptographically random
/// (`getrandom`, same primitive as `auth_cookie`) so it is unguessable by another
/// local process.
pub fn new_session_token() -> anyhow::Result<String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    Ok(hex::encode(buf))
}

// ── per-session token registry (token → agent name) ──────────────────────────────

fn tokens() -> &'static Mutex<HashMap<String, String>> {
    static T: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Bind a freshly-minted token to an agent name (at spawn time). A same-name respawn
/// gets a NEW token, so a stale token from a dead session resolves to nothing.
pub fn register(token: &str, agent: &str) {
    tokens().lock().insert(token.to_string(), agent.to_string());
}

/// Resolve a frame's token to its agent name, or `None` if unknown (forged / stale).
pub fn resolve(token: &str) -> Option<String> {
    tokens().lock().get(token).cloned()
}

/// Drop an agent's token(s) on despawn/delete so the registry doesn't grow across
/// churn and a recycled name can't inherit a dead session's binding.
pub fn forget_agent(agent: &str) {
    tokens().lock().retain(|_, a| a != agent);
}

// ── per-agent evidence buffer ────────────────────────────────────────────────────

fn buffer() -> &'static Mutex<HashMap<String, VecDeque<Evidence>>> {
    static B: OnceLock<Mutex<HashMap<String, VecDeque<Evidence>>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Append one observation for `agent`, bounded drop-oldest at [`BUFFER_CAP`].
pub fn push(agent: &str, ev: Evidence) {
    let mut b = buffer().lock();
    let q = b.entry(agent.to_string()).or_default();
    if q.len() >= BUFFER_CAP {
        q.pop_front();
    }
    q.push_back(ev);
}

/// Drain (remove + return, oldest-first) all evidence for `agent` — the reducer's
/// consume path (Phase B). Deferred consumer: the Phase-B reducer is OUT OF SCOPE for
/// this spike, so nothing in prod drains yet (tests do); kept as the buffer's contract.
#[allow(dead_code)]
pub fn drain(agent: &str) -> Vec<Evidence> {
    let mut b = buffer().lock();
    b.get_mut(agent)
        .map(|q| q.drain(..).collect())
        .unwrap_or_default()
}

/// Non-destructive snapshot (oldest-first) — for samples / debug / tests. Deferred
/// consumer (debug/sample surface) like [`drain`]; not yet read in prod this spike.
#[allow(dead_code)]
pub fn peek(agent: &str) -> Vec<Evidence> {
    buffer()
        .lock()
        .get(agent)
        .map(|q| q.iter().cloned().collect())
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Ingest one frame: validate its token → resolve agent, map the hook → [`Evidence`],
/// push it, and return `(agent, evidence)` for logging/tests. `None` when the token is
/// unknown (rejected — anti-spoof) or the hook carries no state transition. Pure w.r.t.
/// the socket so the round-trip is unit-testable without a real connection.
pub fn ingest_frame(frame: &ShadowFrame) -> Option<(String, Evidence)> {
    let agent = match resolve(&frame.token) {
        Some(a) => a,
        None => {
            tracing::warn!(
                tag = "#shadow-observer",
                event = %frame.hook_event_name,
                "rejected hook frame: unknown session token (anti-spoof)"
            );
            return None;
        }
    };
    let kind = evidence_kind_for_hook(
        &frame.hook_event_name,
        frame.notification_type.as_deref(),
        frame.tool_name.as_deref(),
    )?;
    let ev = Evidence::hook(kind, now_ms());
    push(&agent, ev.clone());
    tracing::info!(
        tag = "#shadow-observer",
        agent = %agent,
        event = %frame.hook_event_name,
        evidence = ?ev.kind,
        "local-plane evidence recorded (authority=Hook, confidence=Confirmed)"
    );
    Some((agent, ev))
}

/// Start the unix-socket event server (one accept loop on a daemon thread). No-op
/// unless [`enabled`]. The unix-socket transport is the only platform-specific part;
/// everything above (token/buffer/mapping) is cross-platform. The fleet runs on
/// macOS/Linux, so Windows is a logged no-op (the flag is OFF there anyway).
pub fn start(home: &Path) {
    if !enabled() {
        return;
    }
    #[cfg(unix)]
    start_unix(home);
    #[cfg(not(unix))]
    {
        let _ = home;
        tracing::info!(
            tag = "#shadow-observer",
            "unix-socket event server unavailable on this platform — local plane disabled"
        );
    }
}

/// Removes a stale socket, binds, and processes one JSON frame per connection. Errors
/// are logged, never fatal — the observer must never wedge the daemon.
#[cfg(unix)]
fn start_unix(home: &Path) {
    let path = socket_path(home);
    let _ = std::fs::remove_file(&path); // clear a stale socket from a prior run
    let listener = match std::os::unix::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(tag = "#shadow-observer", path = %path.display(), error = %e,
                "shadow event server: bind failed — local plane disabled this run");
            return;
        }
    };
    tracing::info!(tag = "#shadow-observer", path = %path.display(),
        "shadow event server listening (local plane)");
    // fire-and-forget: a detached accept loop for an observe-only side-channel; it
    // owns no daemon state and must never block the supervisor. It exits when the
    // daemon process does (the socket is removed on next boot).
    std::thread::Builder::new()
        .name("shadow-event-server".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => handle_conn(stream),
                    Err(e) => tracing::debug!(tag = "#shadow-observer", error = %e,
                        "shadow event server: accept error"),
                }
            }
        })
        .ok();
}

#[cfg(unix)]
fn handle_conn(stream: std::os::unix::net::UnixStream) {
    use std::io::{BufRead, BufReader, Read};
    // One small JSON frame per connection. Bound the read (`Read::take` on the stream,
    // then buffer it) so a wedged client can't pin the accept loop.
    let mut line = String::new();
    let mut reader = BufReader::new(stream.take(64 * 1024));
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    match serde_json::from_str::<ShadowFrame>(line.trim()) {
        Ok(frame) => {
            let _ = ingest_frame(&frame);
        }
        Err(e) => tracing::debug!(tag = "#shadow-observer", error = %e,
            "shadow event server: malformed frame dropped"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::evidence::{Authority, EvidenceKind};
    use super::*;
    use serial_test::serial;

    /// End-to-end over the REAL unix socket transport: a client (the hook emit) writes
    /// one token-authenticated frame line; the server's `handle_conn` reads it, ingests,
    /// and the Evidence lands in the buffer. Proves the socket+token+Evidence path the
    /// spike exists to validate (the live claude hook does exactly this write).
    #[cfg(unix)]
    #[test]
    #[serial(shadow_observer)]
    fn socket_round_trip_delivers_evidence() {
        use std::io::Write;
        use std::os::unix::net::{UnixListener, UnixStream};
        let token = new_session_token().unwrap();
        register(&token, "shadow-sock-agent");
        let dir = std::env::temp_dir().join(format!("agend_shadow_sock_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("shadow-events.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind");
        let server = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                handle_conn(stream);
            }
        });
        let mut stream = UnixStream::connect(&sock).expect("connect");
        let frame = serde_json::json!({
            "token": token,
            "hook_event_name": "UserPromptSubmit",
        });
        stream
            .write_all(format!("{frame}\n").as_bytes())
            .expect("write frame");
        drop(stream);
        server.join().unwrap();
        let evidence = peek("shadow-sock-agent");
        assert_eq!(
            evidence.len(),
            1,
            "frame delivered over the real socket → 1 evidence"
        );
        assert_eq!(evidence[0].kind, EvidenceKind::TurnStarted);
        assert_eq!(evidence[0].authority, Authority::Hook);
        assert_eq!(
            evidence[0].confidence,
            super::evidence::Confidence::Confirmed
        );
        drain("shadow-sock-agent");
        forget_agent("shadow-sock-agent");
        let _ = std::fs::remove_file(&sock);
    }

    fn frame(token: &str, event: &str, tool: Option<&str>) -> ShadowFrame {
        ShadowFrame {
            token: token.to_string(),
            hook_event_name: event.to_string(),
            notification_type: None,
            tool_name: tool.map(str::to_string),
        }
    }

    #[test]
    #[serial(shadow_observer)]
    fn ingest_valid_token_records_evidence() {
        let token = new_session_token().unwrap();
        register(&token, "shadow-test-a");
        let out = ingest_frame(&frame(&token, "PreToolUse", Some("Bash")));
        assert_eq!(
            out.map(|(a, e)| (a, e.kind)),
            Some((
                "shadow-test-a".to_string(),
                EvidenceKind::ToolStarted {
                    name: Some("Bash".to_string())
                }
            ))
        );
        let buffered = drain("shadow-test-a");
        assert_eq!(buffered.len(), 1, "evidence landed in the per-agent buffer");
        assert_eq!(
            buffered[0].kind,
            EvidenceKind::ToolStarted {
                name: Some("Bash".to_string())
            }
        );
        forget_agent("shadow-test-a");
    }

    #[test]
    #[serial(shadow_observer)]
    fn ingest_unknown_token_is_rejected_and_buffers_nothing() {
        // A forged token (never registered) must not produce evidence — the
        // anti-local-spoof property.
        let out = ingest_frame(&frame("deadbeef-not-registered", "UserPromptSubmit", None));
        assert!(out.is_none(), "unknown token rejected");
        assert!(
            drain("shadow-test-b").is_empty(),
            "no evidence buffered for any agent on a forged token"
        );
    }

    #[test]
    #[serial(shadow_observer)]
    fn forget_agent_invalidates_its_token() {
        let token = new_session_token().unwrap();
        register(&token, "shadow-test-c");
        assert_eq!(resolve(&token).as_deref(), Some("shadow-test-c"));
        forget_agent("shadow-test-c");
        assert!(resolve(&token).is_none(), "despawn drops the token binding");
        // A same-name respawn with a new token does not inherit the old one.
        assert!(ingest_frame(&frame(&token, "Stop", None)).is_none());
    }

    #[test]
    #[serial(shadow_observer)]
    fn buffer_is_bounded_drop_oldest() {
        let token = new_session_token().unwrap();
        register(&token, "shadow-test-d");
        for _ in 0..(BUFFER_CAP + 10) {
            ingest_frame(&frame(&token, "PostToolUse", None));
        }
        assert_eq!(peek("shadow-test-d").len(), BUFFER_CAP, "bounded at cap");
        drain("shadow-test-d");
        forget_agent("shadow-test-d");
    }

    #[test]
    fn token_is_256_bit_hex() {
        let t = new_session_token().unwrap();
        assert_eq!(t.len(), 64, "32 bytes → 64 hex chars = 256-bit");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, new_session_token().unwrap(), "tokens are unique");
    }
}
