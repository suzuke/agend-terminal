//! #2413 — opencode HTTP `/event` SSE observer source (Stream plane).
//!
//! opencode is client-server: its native TUI EMBEDS an HTTP server (the same binary /
//! event-bus as `opencode serve`) and is itself a client of it. When the Shadow Observer
//! is enabled (default-ON; `AGEND_SHADOW_OBSERVER=0` disables),
//! [`build_command`](crate::agent) injects `--port N` (an
//! OS-allocated free port, [`alloc_port`]) into the opencode launch so that embedded
//! server is reachable on a KNOWN port; this module subscribes to
//! `http://127.0.0.1:N/event` (Server-Sent Events) and maps opencode's NATIVE session
//! lifecycle → [`Evidence`] (`authority=Stream`) → the SAME per-agent buffer the reducer
//! consumes ([`super::push`]). Parallel to claude (Hook unix socket, `mod.rs`) and codex
//! (rollout file tail, `rollout.rs`); the reducer is unchanged — every plane just fills
//! the buffer.
//!
//! Confirm-first verified (2026-06-24, `SHADOW-OBSERVER-OPENCODE-SPIKE.md`): the TUI
//! embedded server is reachable on the injected port AND a second client can subscribe
//! `/event` concurrently while the TUI runs; a real turn streams
//! `session.status{busy} → … → session.idle` (native idle/working flags, not regex).
//!
//! Cross-platform: raw `std::net::TcpStream` + a PURE chunked/SSE decoder ([`SseDecoder`])
//! — no async runtime, no extra dep. The observer is a sync `std::thread` like `rollout`
//! (the daemon's `reqwest` is async-only). `TcpStream` + a read-timeout poll loop behave
//! on both Unix (`WouldBlock`) and Windows (`TimedOut`).

use super::evidence::{Evidence, EvidenceKind};
use crate::backend::Backend;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

/// Supervisor re-scan cadence: discover newly-spawned / despawned opencode agents.
const SUPERVISE_TICK: Duration = Duration::from_secs(1);
/// Subscriber socket read timeout — bounds how long a quiet (no-event) stream blocks
/// before the loop re-checks its stop flag, so a despawn is honored within ~this window.
const READ_TIMEOUT: Duration = Duration::from_secs(1);
/// Backoff between a dropped subscription and a reconnect attempt (server not up yet,
/// turn-idle disconnect, transient error). Checked against the stop flag.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Current epoch-ms. SSE is live (near-real-time), so a frame is stamped at read time —
/// unlike the rollout tail, there is no append-lag to correct for. Cross-platform
/// (`chrono`, like `rollout`), un-gated.
fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

// ── port discovery: inject `--port N` at spawn, read it back here ──────────────────────

/// Per-agent injected observer port (agent name → port). Mirrors the token registry in
/// `mod.rs`: written at spawn ([`register_port`], overwrites on respawn), cleared on
/// despawn (via [`super::forget_agent`] → [`forget_port`]). A failed-spawn entry is
/// overwritten by the crash-respawn or left as a bounded (fleet-sized) no-op the observer
/// simply can't connect to.
fn ports() -> &'static Mutex<HashMap<String, u16>> {
    static P: OnceLock<Mutex<HashMap<String, u16>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record the `--port` injected into `agent`'s opencode launch so the supervisor knows
/// where to subscribe. Overwrites a prior entry (a respawn gets a fresh port).
pub fn register_port(agent: &str, port: u16) {
    ports().lock().insert(agent.to_string(), port);
}

/// The observer port for `agent`, if one was injected.
fn port_for(agent: &str) -> Option<u16> {
    ports().lock().get(agent).copied()
}

/// Drop `agent`'s port on despawn (called from [`super::forget_agent`]).
pub fn forget_port(agent: &str) {
    ports().lock().remove(agent);
}

/// PURE gate: inject the observer `--port` ONLY when the flag is ON *and* this spawn is
/// opencode. flag-OFF (or any non-opencode backend) → `false` → the opencode launch
/// command is BYTE-IDENTICAL to today (no alloc, no `--port`). This is the load-bearing
/// flag-OFF-safety predicate the regression test pins.
pub fn should_inject(backend: Option<&Backend>, enabled: bool) -> bool {
    enabled && matches!(backend, Some(Backend::OpenCode))
}

/// Allocate a free localhost TCP port (bind `:0`, capture the OS choice, release) so the
/// spawned opencode can take `--port N`. `None` if the OS can't give one → the caller
/// skips injection: the agent spawns WITHOUT `--port` and is simply not observed (never
/// breaks the spawn).
///
/// ⚠ TOCTOU (empirically settled, opt-in only): opencode does ~1s of init before it binds,
/// so the alloc→bind window is ~1s; if N is grabbed in that window opencode HARD-FAILS
/// (`--port N` taken → exit 1, no fallback — verified). That is a <0.01%/spawn, flag-ON
/// opt-in race that SELF-HEALS: the daemon crash-respawns (Fresh) → a new port is
/// allocated → succeeds (two consecutive races are negligible; the crash budget + #2438
/// watchdog backstop). See `SHADOW-OBSERVER-OPENCODE-SPIKE.md`.
pub fn alloc_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
}

// ── pure event → Evidence mapping ──────────────────────────────────────────────────────

/// Map one opencode `/event` SSE frame (the JSON after `data:`) → [`Evidence`]
/// (`authority=Stream`), or `None` for a frame that is not an agent-state transition.
/// PURE — unit-tested against the captured wire shapes, no I/O. `now_ms` is the read-time
/// stamp.
///
/// Mapping (see the spike doc §4 matrix). The granular lifecycle is primary; the headline
/// `session.status` is a coarse backstop. Deliberately IGNORED to keep the bounded buffer
/// meaningful: per-token `message.part.delta` (fires ~100×/turn — would evict real
/// transitions) and the bookkeeping `session.updated` / `message.updated` / `session.diff`.
pub(crate) fn event_to_evidence(frame_json: &str, now_ms: u64) -> Option<Evidence> {
    let v: serde_json::Value = serde_json::from_str(frame_json.trim()).ok()?;
    let ty = v.get("type")?.as_str()?;
    let props = v.get("properties");
    let kind = match ty {
        // Turn boundary.
        "session.next.prompted" => EvidenceKind::TurnStarted,
        "session.idle" => EvidenceKind::TurnEnded { stop_reason: None },
        // Tool lifecycle (carries `tool`/`callID`).
        "session.next.tool.called" => EvidenceKind::ToolStarted {
            name: props
                .and_then(|p| p.get("tool"))
                .and_then(|t| t.as_str())
                .map(str::to_string),
        },
        "session.next.tool.success" | "session.next.tool.failed" => EvidenceKind::ToolEnded,
        // Assistant is producing output (text) / reasoning — both = responding/active.
        "session.next.text.started" | "session.next.reasoning.started" => EvidenceKind::Responding,
        // Native session.status flag — the headline opencode signal. Discriminated by
        // `properties.status.type` (anyOf idle | busy | retry | …).
        "session.status" => {
            match props
                .and_then(|p| p.get("status"))
                .and_then(|s| s.get("type"))
                .and_then(|t| t.as_str())
            {
                Some("busy") => EvidenceKind::Responding,
                Some("idle") => EvidenceKind::PromptReady,
                // retry = opencode is rate-limited/retrying (carries attempt/next); the
                // exact reset instant isn't a reliable absolute, so leave it None.
                Some("retry") => EvidenceKind::RateLimited { retry_at_ms: None },
                _ => return None,
            }
        }
        // Blocked on a tool-permission decision.
        "permission.asked" => EvidenceKind::ApprovalRequired,
        _ => return None,
    };
    Some(Evidence::stream(kind, now_ms))
}

// ── pure chunked + SSE decoder ─────────────────────────────────────────────────────────

/// Find the first occurrence of `needle` in `hay`.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Incremental decoder for a chunked HTTP `text/event-stream` body: feed raw socket bytes,
/// get back complete SSE `data:` payloads (the text after `data:`, with `\r`/leading space
/// trimmed). Handles chunk boundaries AND event boundaries splitting across reads, so a
/// `TcpStream::read` returning an arbitrary byte slice is safe. PURE (no I/O) → fully
/// unit-tested incl. reverse-mutation.
#[derive(Default)]
struct SseDecoder {
    /// Chunked-transfer bytes not yet de-chunked.
    raw: Vec<u8>,
    /// De-chunked body bytes not yet split into complete SSE events.
    body: Vec<u8>,
}

impl SseDecoder {
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.raw.extend_from_slice(bytes);
        self.dechunk();
        self.split_events()
    }

    /// Consume as many complete `<hexsize>\r\n<data>\r\n` chunks as are fully buffered,
    /// appending their data to `self.body`. A partial trailing chunk is left for the next
    /// feed.
    fn dechunk(&mut self) {
        while let Some(nl) = find_subslice(&self.raw, b"\r\n") {
            // chunk-size line (ignore any `;ext` chunk extensions).
            let size_line = &self.raw[..nl];
            let hex = size_line.split(|&b| b == b';').next().unwrap_or(size_line);
            let hex = match std::str::from_utf8(hex) {
                Ok(s) => s.trim(),
                Err(_) => {
                    self.raw.clear();
                    break;
                }
            };
            if hex.is_empty() {
                // Stray/leading CRLF — drop it and continue.
                self.raw.drain(..nl + 2);
                continue;
            }
            let Ok(size) = usize::from_str_radix(hex, 16) else {
                // Malformed size — defensively drop the buffer; the reconnect re-syncs.
                self.raw.clear();
                break;
            };
            let data_start = nl + 2;
            let data_end = data_start + size;
            // Need the data bytes AND the trailing CRLF before consuming the chunk.
            if self.raw.len() < data_end + 2 {
                break;
            }
            self.body.extend_from_slice(&self.raw[data_start..data_end]);
            self.raw.drain(..data_end + 2);
            if size == 0 {
                // Terminal chunk; any trailers are ignored. Stop — the stream is ending.
                break;
            }
        }
    }

    /// Split `self.body` into complete SSE events (separated by a blank line — `\n\n` or
    /// `\r\n\r\n`) and return each event's joined `data:` payload.
    fn split_events(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some((pos, blen)) = event_boundary(&self.body) {
            let event: Vec<u8> = self.body.drain(..pos).collect();
            self.body.drain(..blen);
            let Ok(text) = String::from_utf8(event) else {
                continue;
            };
            // `str::lines` already strips the `\r` of a `\r\n`.
            let data: String = text
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                out.push(data);
            }
        }
        out
    }
}

/// First SSE event boundary (`\n\n` or `\r\n\r\n`, whichever comes first) → (start, len).
fn event_boundary(body: &[u8]) -> Option<(usize, usize)> {
    let crlf = find_subslice(body, b"\r\n\r\n").map(|i| (i, 4));
    let lf = find_subslice(body, b"\n\n").map(|i| (i, 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

// ── subscriber (one per live opencode agent) ───────────────────────────────────────────

/// Subscribe to `agent`'s `/event` until `stop` is set, reconnecting on any disconnect /
/// transient error (the embedded server may not be up at first tick, and goes quiet
/// between turns). Each frame → Evidence → buffer.
fn subscribe_loop(agent: String, port: u16, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        // A connection error / clean EOF just means "retry after a backoff".
        let _ = subscribe_once(&agent, port, &stop);
        sleep_checking(&stop, RECONNECT_BACKOFF);
    }
}

/// One `/event` connection: GET, skip response headers, then decode the chunked SSE body,
/// pushing each mapped frame. Returns on stop, EOF, or error (the caller reconnects).
fn subscribe_once(agent: &str, port: u16, stop: &AtomicBool) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let req = format!(
        "GET /event HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\n\
         Connection: keep-alive\r\n\r\n"
    );
    stream.write_all(req.as_bytes())?;

    let mut dec = SseDecoder::default();
    let mut header_buf: Vec<u8> = Vec::new();
    let mut header_done = false;
    let mut rb = [0u8; 4096];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match stream.read(&mut rb) {
            Ok(0) => return Ok(()), // server closed the stream
            Ok(n) => {
                // Teardown race (#2440 r6): `stop` may have been set WHILE this `read()`
                // blocked, and the old server can return headers + a final SSE frame in
                // this SAME read. Re-check stop BEFORE any decode/push so a torn-down
                // subscriber (despawn / port-change) never injects a late/stale frame under
                // this agent name — which would survive `forget_agent` / a same-name
                // respawn as a phantom transition. (The top-of-loop check only covers a
                // stop observed BEFORE the read returns.)
                if stop.load(Ordering::Relaxed) {
                    return Ok(());
                }
                if header_done {
                    for payload in dec.feed(&rb[..n]) {
                        push_frame(agent, &payload);
                    }
                    continue;
                }
                header_buf.extend_from_slice(&rb[..n]);
                if let Some(pos) = find_subslice(&header_buf, b"\r\n\r\n") {
                    let body = header_buf.split_off(pos + 4);
                    header_done = true;
                    for payload in dec.feed(&body) {
                        push_frame(agent, &payload);
                    }
                    header_buf = Vec::new();
                }
            }
            // Read timeout (Unix: WouldBlock, Windows: TimedOut): loop to re-check `stop`.
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

fn push_frame(agent: &str, payload: &str) {
    if let Some(ev) = event_to_evidence(payload, now_ms()) {
        super::push(agent, ev);
    }
}

/// Sleep `dur` in short slices, returning early if `stop` is set, so a despawn isn't
/// delayed by a full backoff.
fn sleep_checking(stop: &AtomicBool, dur: Duration) {
    let step = Duration::from_millis(100);
    let mut left = dur;
    while left > Duration::ZERO {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let s = step.min(left);
        std::thread::sleep(s);
        left = left.saturating_sub(s);
    }
}

// ── supervisor (fire-and-forget) ───────────────────────────────────────────────────────

/// A live subscriber: the port it's bound to + its stop flag + its join handle (stored for
/// graceful lifecycle, §10.5).
struct Sub {
    port: u16,
    stop: Arc<AtomicBool>,
    _handle: Option<JoinHandle<()>>,
}

/// Spawn the opencode SSE observer supervisor — a fire-and-forget daemon thread (mirrors
/// `rollout::spawn`). No-op unless [`super::enabled`]. Wired into BOTH `run_core` AND
/// `run_app` (the #2434 lesson: the live fleet daemon is app mode).
pub fn spawn(registry: crate::agent::AgentRegistry, _home: PathBuf) {
    if !super::enabled() {
        return;
    }
    // fire-and-forget: a detached supervisor that maintains one `/event` SSE subscriber per
    // live opencode agent. It owns no daemon state, holds no lock across I/O, and exits when
    // the process does; per-agent subscribers are stop-flag controlled (and their handles
    // stored in `subs`) and torn down on despawn / port change. (§10.5)
    let _ = std::thread::Builder::new()
        .name("shadow-opencode-supervisor".into())
        .spawn(move || {
            tracing::info!(
                tag = "#shadow-observer",
                "opencode SSE observer supervisor listening (stream plane)"
            );
            let mut subs: HashMap<String, Sub> = HashMap::new();
            loop {
                supervise_once(&registry, &mut subs);
                std::thread::sleep(SUPERVISE_TICK);
            }
        });
}

/// One supervise cycle: reconcile the subscriber set with the live opencode agents +
/// their injected ports — start new ones, stop those gone (or whose port changed by a
/// respawn).
fn supervise_once(registry: &crate::agent::AgentRegistry, subs: &mut HashMap<String, Sub>) {
    let live: HashMap<String, u16> = live_opencode_ports(registry).into_iter().collect();

    // Stop subscribers whose agent is gone, or whose port changed (respawn → new server).
    subs.retain(|name, sub| match live.get(name) {
        Some(&p) if p == sub.port => true,
        _ => {
            sub.stop.store(true, Ordering::Relaxed);
            false
        }
    });

    // Start a subscriber for each newly-live opencode agent that has an injected port.
    for (name, port) in live {
        if subs.contains_key(&name) {
            continue;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let agent = name.clone();
        // fire-and-forget: per-agent SSE reader, stop-flag controlled; handle stored in
        // `subs` for graceful teardown on despawn. (§10.5)
        let handle = std::thread::Builder::new()
            .name(format!("shadow-oc-sub-{name}"))
            .spawn(move || subscribe_loop(agent, port, stop_thread))
            .ok();
        subs.insert(
            name,
            Sub {
                port,
                stop,
                _handle: handle,
            },
        );
    }
}

/// Snapshot the live opencode agents that have an injected observer port (brief registry
/// lock, released before any I/O).
fn live_opencode_ports(registry: &crate::agent::AgentRegistry) -> Vec<(String, u16)> {
    let reg = crate::agent::lock_registry(registry);
    reg.values()
        .filter(|h| h.backend_command.contains("opencode"))
        .filter_map(|h| {
            let name = h.name.to_string();
            port_for(&name).map(|p| (name, p))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::evidence::{Authority, EvidenceKind};
    use super::*;
    use serial_test::serial;

    fn kind_of(frame: &str) -> Option<EvidenceKind> {
        event_to_evidence(frame, 1_000).map(|e| e.kind)
    }

    #[test]
    fn maps_turn_tool_and_idle_lifecycle() {
        assert_eq!(
            kind_of(r#"{"type":"session.next.prompted","properties":{"sessionID":"ses1"}}"#),
            Some(EvidenceKind::TurnStarted)
        );
        assert_eq!(
            kind_of(r#"{"type":"session.idle","properties":{"sessionID":"ses1"}}"#),
            Some(EvidenceKind::TurnEnded { stop_reason: None })
        );
        assert_eq!(
            kind_of(
                r#"{"type":"session.next.tool.called","properties":{"tool":"bash","callID":"c1"}}"#
            ),
            Some(EvidenceKind::ToolStarted {
                name: Some("bash".into())
            })
        );
        assert_eq!(
            kind_of(r#"{"type":"session.next.tool.success","properties":{"callID":"c1"}}"#),
            Some(EvidenceKind::ToolEnded)
        );
        assert_eq!(
            kind_of(r#"{"type":"session.next.tool.failed","properties":{"callID":"c1"}}"#),
            Some(EvidenceKind::ToolEnded)
        );
        assert_eq!(
            kind_of(r#"{"type":"session.next.text.started","properties":{}}"#),
            Some(EvidenceKind::Responding)
        );
        assert_eq!(
            kind_of(r#"{"type":"session.next.reasoning.started","properties":{}}"#),
            Some(EvidenceKind::Responding)
        );
    }

    #[test]
    fn maps_native_session_status_flag() {
        assert_eq!(
            kind_of(r#"{"type":"session.status","properties":{"status":{"type":"busy"}}}"#),
            Some(EvidenceKind::Responding)
        );
        assert_eq!(
            kind_of(r#"{"type":"session.status","properties":{"status":{"type":"idle"}}}"#),
            Some(EvidenceKind::PromptReady)
        );
        assert_eq!(
            kind_of(
                r#"{"type":"session.status","properties":{"status":{"type":"retry","attempt":1,"next":2}}}"#
            ),
            Some(EvidenceKind::RateLimited { retry_at_ms: None })
        );
        // Unknown status discriminant → not a transition.
        assert_eq!(
            kind_of(r#"{"type":"session.status","properties":{"status":{"type":"weird"}}}"#),
            None
        );
    }

    #[test]
    fn permission_and_noise_mapping() {
        assert_eq!(
            kind_of(r#"{"type":"permission.asked","properties":{}}"#),
            Some(EvidenceKind::ApprovalRequired)
        );
        // Deliberately-ignored noise (would flood the bounded buffer / not a transition).
        assert_eq!(
            kind_of(r#"{"type":"message.part.delta","properties":{}}"#),
            None
        );
        assert_eq!(
            kind_of(r#"{"type":"session.updated","properties":{}}"#),
            None
        );
        assert_eq!(
            kind_of(r#"{"type":"server.connected","properties":{}}"#),
            None
        );
        // Malformed / empty.
        assert_eq!(kind_of("not json"), None);
        assert_eq!(kind_of("{}"), None);
    }

    #[test]
    fn evidence_is_stream_authority_stamped_at_read_time() {
        let ev = event_to_evidence(
            r#"{"type":"session.next.prompted","properties":{"sessionID":"ses1"}}"#,
            7_777,
        )
        .unwrap();
        assert_eq!(ev.authority, Authority::Stream);
        assert_eq!(ev.at_ms, 7_777);
    }

    /// flag-OFF byte-identity pin + reverse-mutation guard: removing the `enabled` check
    /// (or the opencode gate) flips one of these → RED. flag-OFF MUST be no-inject.
    #[test]
    fn should_inject_only_when_flag_on_and_opencode() {
        assert!(should_inject(Some(&Backend::OpenCode), true));
        // flag-OFF → never inject (the load-bearing byte-identity case).
        assert!(!should_inject(Some(&Backend::OpenCode), false));
        // Wrong backend → never inject, even flag-ON.
        assert!(!should_inject(Some(&Backend::ClaudeCode), true));
        assert!(!should_inject(Some(&Backend::Codex), true));
        assert!(!should_inject(None, true));
    }

    #[test]
    fn alloc_port_returns_a_usable_port() {
        let p = alloc_port().expect("an ephemeral port");
        assert!(p > 0);
        // The port was released, so it must be re-bindable.
        std::net::TcpListener::bind(("127.0.0.1", p)).expect("port is free after alloc");
    }

    #[test]
    #[serial(shadow_observer)]
    fn port_registry_roundtrip_and_forget() {
        register_port("ocx", 54321);
        assert_eq!(port_for("ocx"), Some(54321));
        // Respawn overwrites.
        register_port("ocx", 54322);
        assert_eq!(port_for("ocx"), Some(54322));
        forget_port("ocx");
        assert_eq!(port_for("ocx"), None);
    }

    // ── SseDecoder (chunked + SSE) — boundary cases drive reverse-mutation ──────────────

    /// Encode `data: <json>\n\n` as one HTTP chunk (`<hexsize>\r\n<bytes>\r\n`).
    fn chunk(payload: &str) -> Vec<u8> {
        let body = format!("data: {payload}\n\n");
        format!("{:x}\r\n{}\r\n", body.len(), body).into_bytes()
    }

    #[test]
    fn decodes_single_event_in_one_chunk() {
        let mut d = SseDecoder::default();
        let out = d.feed(&chunk(r#"{"type":"session.idle"}"#));
        assert_eq!(out, vec![r#"{"type":"session.idle"}"#.to_string()]);
    }

    #[test]
    fn decodes_multiple_events_in_one_chunk() {
        let mut d = SseDecoder::default();
        let mut bytes = chunk(r#"{"type":"a"}"#);
        bytes.extend(chunk(r#"{"type":"b"}"#));
        let out = d.feed(&bytes);
        assert_eq!(out, vec![r#"{"type":"a"}"#, r#"{"type":"b"}"#]);
    }

    #[test]
    fn reassembles_a_chunk_split_across_two_feeds() {
        let mut d = SseDecoder::default();
        let full = chunk(r#"{"type":"session.next.prompted"}"#);
        let split = full.len() / 2;
        assert!(
            d.feed(&full[..split]).is_empty(),
            "partial chunk yields nothing"
        );
        let out = d.feed(&full[split..]);
        assert_eq!(out, vec![r#"{"type":"session.next.prompted"}"#.to_string()]);
    }

    #[test]
    fn reassembles_an_event_spanning_two_chunks() {
        // SSE event whose bytes arrive as two separate HTTP chunks.
        let mut d = SseDecoder::default();
        let c1 = b"a\r\ndata: {\"ty\r\n".to_vec(); // 0xa = 10 bytes: "data: {\"ty"
        let mut bytes = c1;
        let part2 = "pe\":\"x\"}\n\n";
        bytes.extend(format!("{:x}\r\n{}\r\n", part2.len(), part2).into_bytes());
        let out = d.feed(&bytes);
        assert_eq!(out, vec![r#"{"type":"x"}"#.to_string()]);
    }

    /// The chunk's data bytes are all present but its trailing CRLF has NOT arrived: the
    /// decoder must WAIT (emit nothing, never panic) until the CRLF completes the chunk.
    /// Pins the `data_end + 2` guard load-bearing (a `< data_end` neuter panics here).
    #[test]
    fn waits_for_trailing_crlf_before_consuming_chunk() {
        let mut d = SseDecoder::default();
        let full = chunk(r#"{"type":"x"}"#);
        // Everything except the final trailing "\r\n".
        let head = &full[..full.len() - 2];
        assert!(
            d.feed(head).is_empty(),
            "data present but trailing CRLF missing → wait, no emit"
        );
        let out = d.feed(&full[full.len() - 2..]);
        assert_eq!(out, vec![r#"{"type":"x"}"#.to_string()]);
    }

    #[test]
    fn handles_crlf_event_boundary_and_terminal_chunk() {
        let mut d = SseDecoder::default();
        let body = "data: {\"type\":\"session.status\"}\r\n\r\n";
        let mut bytes = format!("{:x}\r\n{}\r\n", body.len(), body).into_bytes();
        bytes.extend(b"0\r\n\r\n"); // terminal chunk
        let out = d.feed(&bytes);
        assert_eq!(out, vec![r#"{"type":"session.status"}"#.to_string()]);
    }

    /// #2440 r6 teardown race: `stop` is set WHILE the subscriber blocks in `read()`, and
    /// the old server then returns headers + a final SSE frame in that same read. A
    /// torn-down subscriber must NOT decode/push that late frame (it would survive
    /// `forget_agent` / a same-name respawn as a phantom transition). Deterministic: the
    /// server drains the GET (proving the subscriber is now blocked in read) BEFORE setting
    /// stop, so the post-read stop-check is what must short-circuit. Reverse-mutation:
    /// removing that post-read check makes this RED (a phantom TurnStarted is pushed).
    #[test]
    #[serial(shadow_observer)]
    fn stopped_subscriber_does_not_push_late_frames_after_stop() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let agent = "oc_stop_race";
        super::super::forget_agent(agent); // clean slate

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));

        let stop_srv = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // Drain the GET request — once read, the subscriber is past its top-of-loop
            // stop check and blocked in read() waiting for the response.
            let mut req = Vec::new();
            let mut b = [0u8; 512];
            while let Ok(n) = sock.read(&mut b) {
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&b[..n]);
                if find_subslice(&req, b"\r\n\r\n").is_some() {
                    break;
                }
            }
            // Teardown happens NOW (despawn / port-change), while the subscriber blocks.
            stop_srv.store(true, Ordering::Relaxed);
            // The old server flushes headers + a final SSE frame in one write.
            let body = "data: {\"type\":\"session.next.prompted\"}\n\n";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
            // drop(sock) closes the stream.
        });

        let _ = subscribe_once(agent, port, &stop);
        server.join().unwrap();

        let buffered = super::super::peek(agent);
        assert!(
            buffered.is_empty(),
            "a stopped subscriber must NOT push a frame received after stop, got {buffered:?}"
        );
        super::super::forget_agent(agent);
    }
}
