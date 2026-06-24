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
pub mod rollout;

use evidence::Evidence;
// #2433: only the unix-gated ingest path uses these — gate the imports to match so they
// aren't unused on a non-unix prod build (`evidence_kind_for_hook` feeds `ingest_frame`;
// serde derives only `ShadowFrame`, the wire frame).
#[cfg(any(unix, test))]
use evidence::evidence_kind_for_hook;
use parking_lot::Mutex;
#[cfg(any(unix, test))]
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Per-agent evidence buffer cap — drop-oldest beyond this. A spike bound; the
/// reducer drains far faster than evidence fires, so this only guards a never-drained
/// agent from unbounded growth.
// #2413 Phase D: now used cross-platform — the codex `rollout` plane (std::fs tail) pushes
// here too, not just the `#[cfg(unix)]` hook socket. So `push`/`BUFFER_CAP` are no longer
// unix-only and the #2433 `cfg(any(unix, test))` gate is removed (genuinely live on all
// platforms via the rollout tailer).
const BUFFER_CAP: usize = 256;

/// One wire frame the hook emit writes to the socket (a single JSON line). Mirrors
/// the existing `hook-event` payload fields + the session token.
#[cfg(any(unix, test))]
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
/// #2433: only the unix socket-ingest path (+ tests) resolves; gate it like the rest of
/// the ingestion plumbing so it isn't dead on non-unix prod.
#[cfg(any(unix, test))]
pub fn resolve(token: &str) -> Option<String> {
    tokens().lock().get(token).cloned()
}

/// Drop an agent's token(s) on despawn/delete so the registry doesn't grow across
/// churn and a recycled name can't inherit a dead session's binding. Also clears
/// the per-agent Evidence buffer + reducer runtime so a same-name respawn starts
/// CLEAN (no inherited open episode / stale evidence) — the despawn hook is the
/// lifecycle point that keeps the name-keyed reducer state honest.
pub fn forget_agent(agent: &str) {
    tokens().lock().retain(|_, a| a != agent);
    buffer().lock().remove(agent);
    runtimes().lock().remove(agent);
}

// ── per-agent evidence buffer ────────────────────────────────────────────────────

fn buffer() -> &'static Mutex<HashMap<String, VecDeque<Evidence>>> {
    static B: OnceLock<Mutex<HashMap<String, VecDeque<Evidence>>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Append one observation for `agent`, bounded drop-oldest at [`BUFFER_CAP`]. #2413
/// Phase D: pushed by BOTH the unix hook-ingest path (claude) AND the cross-platform
/// `rollout` tailer (codex), so it is no longer unix-gated.
pub fn push(agent: &str, ev: Evidence) {
    let mut b = buffer().lock();
    let q = b.entry(agent.to_string()).or_default();
    if q.len() >= BUFFER_CAP {
        q.pop_front();
    }
    q.push_back(ev);
}

/// Drain (remove + return, oldest-first) all evidence for `agent` — the reducer's
/// consume path (Phase B). Consumed by [`observe`] (the per-tick driver folds the
/// drained evidence into the agent's persistent reducer runtime).
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

// ── per-agent reducer runtime (Phase B driver) ─────────────────────────────────────

/// Persistent per-agent reducer accumulators. Keyed by NAME (consistent with the
/// Evidence buffer + token registry above); [`forget_agent`] drops the entry on despawn
/// so a same-name respawn starts with a fresh [`reducer::AgentRuntime`].
fn runtimes() -> &'static Mutex<HashMap<String, reducer::AgentRuntime>> {
    static R: OnceLock<Mutex<HashMap<String, reducer::AgentRuntime>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fold `agent`'s buffered hook Evidence into its persistent reducer runtime, then
/// derive the [`reducer::ObservedStatus`] from the current screen + liveness snapshot.
/// The per-tick driver ([`crate::daemon::per_tick::shadow_observe`]) calls this under the
/// `AGEND_SHADOW_OBSERVER` flag and hangs the result on `AgentCore.observed_status`
/// (purely ADDITIVE — never rewrites `agent_state`). Draining here is the buffer's sole
/// consume path: each tick takes the new hook events and advances the episode model.
pub fn observe(
    agent: &str,
    screen: reducer::ScreenSignal,
    live: &reducer::Liveness,
    now_ms: u64,
) -> reducer::ObservedStatus {
    let evidence = drain(agent);
    let mut rts = runtimes().lock();
    let rt = rts.entry(agent.to_string()).or_default();
    for ev in &evidence {
        rt.ingest(ev);
    }
    rt.observe(screen, live, now_ms)
}

#[cfg(any(unix, test))]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Ingest one frame: validate its token → resolve agent, map the hook → [`Evidence`],
/// push it, and return `(agent, evidence)` for logging/tests. `None` when the token is
/// unknown (rejected — anti-spoof) or the hook carries no state transition. Pure w.r.t.
/// the socket so the round-trip is unit-testable without a real connection. #2433: gated
/// like the rest of the ingestion plumbing (its only prod caller is the unix socket).
#[cfg(any(unix, test))]
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

/// Per-connection read deadline for the hook-event socket. The accept loop processes
/// connections SEQUENTIALLY, so without a wall-clock bound a single client that connects
/// and then never sends a newline would block `read_line` indefinitely and PIN the loop —
/// starving all subsequent hook delivery (#2433 r6). A starved hook plane in turn makes
/// the dropped-terminal-hook phantom-stick MORE likely. Generous (a real hook writes one
/// line + closes in well under a ms); the bound only guards the wedged-client failure mode.
#[cfg(unix)]
const SOCKET_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(unix)]
fn handle_conn(stream: std::os::unix::net::UnixStream) {
    use std::io::{BufRead, BufReader, Read};
    // One small JSON frame per connection, with TWO independent bounds so a wedged client
    // can never pin the (sequential) accept loop: a wall-clock read deadline
    // (`set_read_timeout` — a client that connects but never sends a newline) AND a byte
    // cap (`Read::take` — a client that floods bytes without a newline). #2433 r6.
    let _ = stream.set_read_timeout(Some(SOCKET_READ_TIMEOUT));
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
    // `Authority` is referenced only by the `#[cfg(unix)]` socket tests, so importing it
    // unconditionally would be an unused-import error on a non-unix test build (#2433) —
    // those tests qualify it inline (`super::evidence::Authority`) instead.
    use super::evidence::EvidenceKind;
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
        assert_eq!(evidence[0].authority, super::evidence::Authority::Hook);
        assert_eq!(
            evidence[0].confidence,
            super::evidence::Confidence::Confirmed
        );
        drain("shadow-sock-agent");
        forget_agent("shadow-sock-agent");
        let _ = std::fs::remove_file(&sock);
    }

    /// #2433 r6 (②): a client that connects but NEVER sends a newline must not pin the
    /// (sequential) accept loop forever — the per-conn read deadline unblocks it, and the
    /// loop goes on to ingest the next real frame. Without `set_read_timeout`, the first
    /// `handle_conn` blocks indefinitely and this test hangs (caught as a nextest
    /// slow-timeout). Takes ~`SOCKET_READ_TIMEOUT` by design.
    #[cfg(unix)]
    #[test]
    #[serial(shadow_observer)]
    fn handle_conn_read_deadline_unblocks_a_silent_client() {
        use std::io::Write;
        use std::os::unix::net::{UnixListener, UnixStream};
        let token = new_session_token().unwrap();
        register(&token, "shadow-deadline-agent");
        let dir =
            std::env::temp_dir().join(format!("agend_shadow_deadline_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("shadow-events.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind");
        // Server: first conn is SILENT (must return via the deadline, not hang), then the
        // second conn carries a real frame (must still ingest — proves the loop recovered).
        let server = std::thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                handle_conn(s);
            }
            if let Ok((s, _)) = listener.accept() {
                handle_conn(s);
            }
        });
        // Silent client connects FIRST (accepted first) and writes nothing; held open
        // across the deadline so the first `handle_conn` truly never sees a newline.
        let silent = UnixStream::connect(&sock).expect("connect silent");
        // Real client queued behind it; its bytes wait in the socket until the loop
        // recovers from the silent conn and accepts again.
        let mut real = UnixStream::connect(&sock).expect("connect real");
        let frame = serde_json::json!({"token": token, "hook_event_name": "UserPromptSubmit"});
        real.write_all(format!("{frame}\n").as_bytes())
            .expect("write real frame");
        drop(real);
        server.join().unwrap();
        drop(silent);
        let evidence = peek("shadow-deadline-agent");
        assert_eq!(
            evidence.len(),
            1,
            "loop recovered from the silent client and ingested the real frame"
        );
        assert_eq!(evidence[0].kind, EvidenceKind::TurnStarted);
        drain("shadow-deadline-agent");
        forget_agent("shadow-deadline-agent");
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
