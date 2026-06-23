//! #2413 Phase 2 Slice 1 — per-agent loopback HTTP CONNECT proxy (engine + lifecycle).
//!
//! A blocking `std::net` thread-per-tunnel **HTTP CONNECT** proxy that **NEVER
//! decrypts** — it only observes `CONNECT host:port` (turn start) / `CLOSE`
//! (turn end) + opaque byte counts. Deliberately NOT tokio: a byte-tunnel needs
//! no async, and the codebase has a documented shared-`current_thread`
//! nested-`block_on` hazard (#1476). This mirrors `api::serve`'s thread-per-conn
//! loopback-server pattern.
//!
//! ## Slice 1 scope (this file) — ZERO agent risk
//! Engine (`ConnectProxy`) + a **flag-gated** daemon supervisor that manages the
//! per-agent proxy *lifecycle* (start / health / reap). The flag
//! (`AGEND_CONNECT_PROXY`) is **default-OFF**, so by default the supervisor never
//! runs and nothing changes. Even when ON, **nothing injects `HTTPS_PROXY` into
//! any agent** (that is Slice 2) — the proxies sit idle, so no agent ever routes
//! through one. The engine is independently testable (start a proxy, tunnel real
//! bytes through it, assert the CONNECT/CLOSE/byte accounting + health).
//!
//! Slice 2 (gated on operator design-review) wires spawn-env injection +
//! reconcile into `AgentCore::api_activity`; Slice 3 adds the fallback supervisor.

use crate::agent::AgentRegistry;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Default-OFF flag gating the whole Phase-2 proxy (mirrors the existing
/// `AGEND_ENV_ISOLATION` / `AGEND_HOOK_STATE_POC` env-flag convention).
const FLAG_ENV: &str = "AGEND_CONNECT_PROXY";
/// Max concurrent tunnels per proxy (mirrors `api::serve`'s `API_MAX_CONNS`).
const MAX_TUNNELS: usize = 64;
/// Supervisor reconcile/health cadence.
const SUPERVISOR_TICK: Duration = Duration::from_secs(5);
/// Timeout dialing the upstream target on a CONNECT.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout reading the CONNECT request headers from the client (anti-hang).
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on the CONNECT request header bytes we'll buffer (anti-DoS).
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Is the Phase-2 CONNECT proxy enabled? **Default OFF** — flipping this OFF is
/// the instant rollback (no proxy threads spawn; behaviour byte-identical).
pub fn enabled() -> bool {
    std::env::var(FLAG_ENV).as_deref() == Ok("1")
}

// ── engine ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Stats {
    accepted: AtomicU64,
    established: AtomicU64,
    failed: AtomicU64,
    bytes_up: AtomicU64,
    bytes_down: AtomicU64,
    active: AtomicUsize,
}

/// A point-in-time snapshot of a proxy's liveness + counters (the input the
/// Slice-3 fallback supervisor will key on; Slice 1 just surfaces it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyHealth {
    /// Accept-loop thread still running (not finished/panicked).
    pub alive: bool,
    pub accepted: u64,
    pub established: u64,
    pub failed: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub active: usize,
}

/// A running per-agent CONNECT proxy: a loopback listener + accept loop that
/// spawns one byte-tunnel worker per client connection. Dropping it shuts down.
pub struct ConnectProxy {
    port: u16,
    shutdown: Arc<AtomicBool>,
    stats: Arc<Stats>,
    accept_handle: Option<JoinHandle<()>>,
}

impl ConnectProxy {
    /// Bind `127.0.0.1:0` and start the accept loop. `label` (the agent name)
    /// is only used for structured-log attribution.
    pub fn start(label: impl Into<String>) -> std::io::Result<Self> {
        let listener = crate::ipc::bind_loopback()?;
        let port = crate::ipc::local_port(&listener);
        // Non-blocking so the accept loop can poll the shutdown flag instead of
        // blocking forever in `accept()`.
        listener.set_nonblocking(true)?;
        let label = label.into();
        let shutdown = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        // fire-and-forget: graceful-join variant — the JoinHandle is stored in
        // `accept_handle` and joined in `shutdown()`/`Drop` (shutdown trigger =
        // the `shutdown` AtomicBool). Marker present per §10.5 / spawn_rationale_audit.
        let accept_handle = std::thread::Builder::new()
            .name("connect_proxy".into())
            .spawn({
                let shutdown = Arc::clone(&shutdown);
                let stats = Arc::clone(&stats);
                // `label` (+ `listener`) move into the accept loop — the struct
                // does not retain them. The JoinHandle IS stored (`accept_handle`)
                // and joined in `shutdown()`/`Drop` — §10.5 graceful-join.
                move || accept_loop(&listener, &shutdown, &stats, &label)
            })?;
        Ok(Self {
            port,
            shutdown,
            stats,
            accept_handle: Some(accept_handle),
        })
    }

    /// The loopback port this proxy listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Liveness + counters snapshot.
    pub fn health(&self) -> ProxyHealth {
        let alive = self
            .accept_handle
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false);
        ProxyHealth {
            alive,
            accepted: self.stats.accepted.load(Ordering::Relaxed),
            established: self.stats.established.load(Ordering::Relaxed),
            failed: self.stats.failed.load(Ordering::Relaxed),
            bytes_up: self.stats.bytes_up.load(Ordering::Relaxed),
            bytes_down: self.stats.bytes_down.load(Ordering::Relaxed),
            active: self.stats.active.load(Ordering::Relaxed),
        }
    }

    /// Self-CONNECT probe: open a client connection to THIS proxy and `CONNECT`
    /// to `target`, returning true iff the proxy answered `200 Connection
    /// Established` (proves parse + dial + reply end-to-end). The caller MUST
    /// pass a **loopback** target (the daemon supervisor uses a loopback beacon)
    /// — never an external host, or network jitter would false-fail health and
    /// could spuriously trip the (future) fallback respawn.
    pub fn self_probe(&self, target: &str) -> bool {
        self_probe_via(self.port, target)
    }

    /// Signal the accept loop to stop and join it. Idempotent.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.accept_handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ConnectProxy {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(listener: &TcpListener, shutdown: &AtomicBool, stats: &Arc<Stats>, label: &str) {
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stats.active.load(Ordering::Relaxed) >= MAX_TUNNELS {
                    tracing::warn!(proxy = label, "connect-proxy at tunnel cap; dropping conn");
                    continue; // stream dropped → client connection reset
                }
                stats.accepted.fetch_add(1, Ordering::Relaxed);
                let stats = Arc::clone(stats);
                let tunnel_label = label.to_string();
                // fire-and-forget: one short-lived byte-tunnel worker per client
                // connection; it terminates when either socket closes. Liveness
                // is tracked via `stats.active`; losing the thread on daemon exit
                // is harmless (the client socket dies with the process).
                let spawned = std::thread::Builder::new()
                    .name("connect_tunnel".into())
                    .spawn(move || {
                        stats.active.fetch_add(1, Ordering::Relaxed);
                        handle_tunnel(stream, &stats, &tunnel_label);
                        stats.active.fetch_sub(1, Ordering::Relaxed);
                    });
                if spawned.is_err() {
                    tracing::warn!(proxy = label, "failed to spawn connect_tunnel thread");
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                tracing::debug!(proxy = label, error = %e, "connect-proxy accept error");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Handle one client connection: read the CONNECT request, dial the target,
/// reply 200, then splice bytes opaquely both directions (counting up/down).
fn handle_tunnel(client: TcpStream, stats: &Stats, label: &str) {
    // Listener was non-blocking, so accepted sockets inherit it on some
    // platforms — force blocking for the request read + tunnel.
    let _ = client.set_nonblocking(false);
    let _ = client.set_read_timeout(Some(HEADER_READ_TIMEOUT));

    let headers = match read_request_headers(&client) {
        Ok(h) => h,
        Err(_) => {
            stats.failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let Some((host, port)) = parse_connect_request(&headers) else {
        let _ = write_status(&client, "400 Bad Request");
        stats.failed.fetch_add(1, Ordering::Relaxed);
        return;
    };

    let target = format!("{host}:{port}");
    let upstream = match dial(&target) {
        Ok(s) => s,
        Err(e) => {
            let _ = write_status(&client, "502 Bad Gateway");
            stats.failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(proxy = label, host = %host, port, error = %e, "connect-proxy 502 dial failed");
            return;
        }
    };

    if write_status(&client, "200 Connection Established").is_err() {
        stats.failed.fetch_add(1, Ordering::Relaxed);
        return;
    }
    stats.established.fetch_add(1, Ordering::Relaxed);
    tracing::info!(proxy = label, host = %host, port, "connect-proxy CONNECT");

    // Tunnel is unbounded-duration: clear the header read timeout.
    let _ = client.set_read_timeout(None);
    let (up, down) = splice(client, upstream);
    stats.bytes_up.fetch_add(up, Ordering::Relaxed);
    stats.bytes_down.fetch_add(down, Ordering::Relaxed);
    tracing::info!(proxy = label, host = %host, port, up, down, "connect-proxy CLOSE");
}

/// Read bytes from `client` until the end-of-headers `\r\n\r\n`, bounded.
fn read_request_headers(mut client: &TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while buf.len() < MAX_HEADER_BYTES {
        let n = client.read(&mut byte)?;
        if n == 0 {
            break; // EOF before end-of-headers
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Parse the CONNECT request line → `(host, port)`. Returns `None` for any
/// non-CONNECT verb or malformed authority. Handles bracketed IPv6.
fn parse_connect_request(headers: &str) -> Option<(String, u16)> {
    let first = headers.lines().next()?;
    let mut parts = first.split_whitespace();
    if !parts.next()?.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let authority = parts.next()?;
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        rest.split_once("]:")? // [2001:db8::1]:443
    } else {
        authority.rsplit_once(':')? // host:443
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?))
}

fn dial(target: &str) -> std::io::Result<TcpStream> {
    let addr = target.to_socket_addrs_first().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "unresolved target")
    })?;
    TcpStream::connect_timeout(&addr, DIAL_TIMEOUT)
}

fn write_status(mut client: &TcpStream, status: &str) -> std::io::Result<()> {
    client.write_all(format!("HTTP/1.1 {status}\r\n\r\n").as_bytes())
}

/// Splice bytes both directions until either side closes; returns `(up, down)`
/// byte counts. `up` = client→server, `down` = server→client.
fn splice(client: TcpStream, server: TcpStream) -> (u64, u64) {
    let (Ok(mut client_r), Ok(mut server_w), Ok(mut server_r), Ok(mut client_w)) = (
        client.try_clone(),
        server.try_clone(),
        server.try_clone(),
        client.try_clone(),
    ) else {
        return (0, 0);
    };

    // up: client → server.
    // fire-and-forget: graceful-join variant — the JoinHandle (`up`) is joined a
    // few lines below; shutdown trigger = either socket closing (io::copy EOF) or
    // the down-copy's `client_w.shutdown(Both)`. Marker per §10.5.
    let up = std::thread::Builder::new()
        .name("connect_splice_up".into())
        .spawn(move || {
            let n = std::io::copy(&mut client_r, &mut server_w).unwrap_or(0);
            let _ = server_w.shutdown(Shutdown::Both); // unblock the down copy
            n
        });

    // down: server → client (this thread).
    let down_n = std::io::copy(&mut server_r, &mut client_w).unwrap_or(0);
    let _ = client_w.shutdown(Shutdown::Both); // unblock the up copy

    let up_n = match up {
        Ok(h) => h.join().unwrap_or(0),
        Err(_) => 0,
    };
    (up_n, down_n)
}

/// Self-probe: connect to `127.0.0.1:proxy_port`, send a `CONNECT target`, and
/// return true iff the reply starts with `HTTP/1.1 200`.
fn self_probe_via(proxy_port: u16, target: &str) -> bool {
    let Ok(mut s) = TcpStream::connect(("127.0.0.1", proxy_port)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(DIAL_TIMEOUT));
    let _ = s.set_write_timeout(Some(DIAL_TIMEOUT));
    if s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .is_err()
    {
        return false;
    }
    let mut resp = [0u8; 16];
    match s.read(&mut resp) {
        Ok(n) => resp[..n].starts_with(b"HTTP/1.1 200"),
        Err(_) => false,
    }
}

/// Tiny helper: first resolved socket address for a `host:port` string.
trait FirstSocketAddr {
    fn to_socket_addrs_first(&self) -> Option<std::net::SocketAddr>;
}
impl FirstSocketAddr for str {
    fn to_socket_addrs_first(&self) -> Option<std::net::SocketAddr> {
        use std::net::ToSocketAddrs;
        self.to_socket_addrs().ok()?.next()
    }
}

// ── daemon supervisor (flag-gated lifecycle; default-OFF) ─────────────────────

/// Start the per-agent proxy supervisor IFF the flag is ON. Default-OFF → this
/// returns immediately and no proxy threads ever spawn (byte-identical to
/// today). Even when ON, **no agent is injected with `HTTPS_PROXY`** (Slice 2),
/// so the managed proxies sit idle — zero agent risk. Call once at daemon boot.
pub fn maybe_spawn_supervisor(registry: AgentRegistry) {
    if !enabled() {
        return;
    }
    // fire-and-forget: flag-gated lifecycle supervisor; read-only management of
    // idle proxies. Terminates on process exit; owns no state another thread
    // must join. §10.5.
    let spawned = std::thread::Builder::new()
        .name("connect_proxy_supervisor".into())
        .spawn(move || supervisor_loop(&registry));
    if spawned.is_err() {
        tracing::warn!("failed to spawn connect_proxy_supervisor thread");
    }
}

fn supervisor_loop(registry: &AgentRegistry) {
    // Loopback health beacon: the self-probe target (lead's Slice-1/3 guidance —
    // probe a loopback target, never an external host, so network jitter can't
    // false-fail health). Accept-and-drop is enough: a successful CONNECT to it
    // proves the proxy parses + dials + replies.
    let beacon = match HealthBeacon::start() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "connect_proxy supervisor: health beacon failed; supervisor exiting");
            return;
        }
    };
    tracing::info!(
        beacon_port = beacon.port(),
        "connect_proxy supervisor started (flag ON)"
    );
    let mut proxies: HashMap<String, ConnectProxy> = HashMap::new();
    loop {
        let live = live_agent_names(registry);
        reconcile(&mut proxies, &live);
        for (name, proxy) in &proxies {
            let h = proxy.health();
            let probe_ok = proxy.self_probe(&beacon.addr());
            tracing::debug!(
                agent = name,
                port = proxy.port(),
                alive = h.alive,
                probe_ok,
                established = h.established,
                failed = h.failed,
                "connect-proxy health"
            );
        }
        std::thread::sleep(SUPERVISOR_TICK);
    }
}

/// Start a proxy for every live agent that lacks one; drop proxies for agents
/// that have gone (their `ConnectProxy::drop` shuts the listener down).
fn reconcile(proxies: &mut HashMap<String, ConnectProxy>, live: &HashSet<String>) {
    proxies.retain(|name, _| live.contains(name));
    for name in live {
        if !proxies.contains_key(name) {
            match ConnectProxy::start(name.clone()) {
                Ok(p) => {
                    tracing::info!(agent = name, port = p.port(), "connect-proxy started");
                    proxies.insert(name.clone(), p);
                }
                Err(e) => {
                    tracing::warn!(agent = name, error = %e, "connect-proxy start failed")
                }
            }
        }
    }
}

fn live_agent_names(registry: &AgentRegistry) -> HashSet<String> {
    crate::agent::lock_registry(registry)
        .values()
        .map(|h| h.name.to_string())
        .collect()
}

/// A loopback accept-and-drop listener used purely as a self-probe target.
struct HealthBeacon {
    port: u16,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl HealthBeacon {
    fn start() -> std::io::Result<Self> {
        let listener = crate::ipc::bind_loopback()?;
        let port = crate::ipc::local_port(&listener);
        listener.set_nonblocking(true)?;
        let shutdown = Arc::new(AtomicBool::new(false));
        // fire-and-forget: graceful-join variant — the JoinHandle is stored in
        // `handle` and joined in `Drop` (shutdown trigger = the `shutdown`
        // AtomicBool). Marker per §10.5 / spawn_rationale_audit.
        let handle = std::thread::Builder::new()
            .name("connect_proxy_beacon".into())
            .spawn({
                let shutdown = Arc::clone(&shutdown);
                move || {
                    while !shutdown.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => drop(stream), // accept + drop
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(100));
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(100)),
                        }
                    }
                }
            })?;
        Ok(Self {
            port,
            shutdown,
            handle: Some(handle),
        })
    }
    fn port(&self) -> u16 {
        self.port
    }
    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for HealthBeacon {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A loopback TCP echo server (accept one conn, echo bytes back). Returns
    /// its `host:port` + a JoinHandle. Used as a representative tunnel target —
    /// the proxy is protocol-opaque, so an echo proves the byte-splice carries
    /// arbitrary payloads (which is all TLS is to the proxy).
    fn spawn_echo() -> (String, JoinHandle<()>) {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind echo");
        let addr = format!("127.0.0.1:{}", l.local_addr().expect("addr").port());
        let h = std::thread::spawn(move || {
            if let Ok((mut s, _)) = l.accept() {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf) {
                    if n == 0 || s.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        });
        (addr, h)
    }

    /// Drive a CONNECT through the proxy and return the connected client stream
    /// (post-200), or None on non-200.
    fn connect_through(proxy_port: u16, target: &str) -> Option<TcpStream> {
        let mut s = TcpStream::connect(("127.0.0.1", proxy_port)).expect("dial proxy");
        s.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .expect("write CONNECT");
        let mut resp = [0u8; 39];
        let n = s.read(&mut resp).expect("read status");
        if resp[..n].starts_with(b"HTTP/1.1 200") {
            Some(s)
        } else {
            None
        }
    }

    #[test]
    fn parse_connect_request_variants() {
        assert_eq!(
            parse_connect_request("CONNECT api.anthropic.com:443 HTTP/1.1\r\n\r\n"),
            Some(("api.anthropic.com".to_string(), 443))
        );
        assert_eq!(
            parse_connect_request("connect chatgpt.com:443 HTTP/1.1"),
            Some(("chatgpt.com".to_string(), 443))
        );
        assert_eq!(
            parse_connect_request("CONNECT [2001:db8::1]:443 HTTP/1.1"),
            Some(("2001:db8::1".to_string(), 443))
        );
        // non-CONNECT verb / malformed → None (the proxy only does CONNECT).
        assert_eq!(parse_connect_request("GET / HTTP/1.1"), None);
        assert_eq!(parse_connect_request("CONNECT noport HTTP/1.1"), None);
        assert_eq!(parse_connect_request(""), None);
    }

    #[test]
    fn enabled_is_default_off() {
        // The flag accessor compares to "1"; absent/other → false. (We don't
        // mutate the process env here — other tests run concurrently.)
        if std::env::var(FLAG_ENV).is_err() {
            assert!(!enabled());
        }
    }

    #[test]
    fn start_binds_a_nonzero_loopback_port() {
        let p = ConnectProxy::start("t").expect("start");
        assert!(p.port() > 0);
        assert!(p.health().alive);
    }

    #[test]
    fn tunnels_real_bytes_and_counts_them() {
        let (echo_addr, echo_h) = spawn_echo();
        let proxy = ConnectProxy::start("agent-x").expect("start");
        let mut client = connect_through(proxy.port(), &echo_addr).expect("CONNECT 200");

        let payload = b"opaque-tls-like-bytes-1234567890";
        client.write_all(payload).expect("write payload");
        let mut got = vec![0u8; payload.len()];
        client.read_exact(&mut got).expect("read echo");
        assert_eq!(&got, payload, "byte-tunnel must carry payload verbatim");

        client.shutdown(Shutdown::Both).ok();
        // Give the tunnel worker a moment to record CLOSE byte counts.
        for _ in 0..50 {
            let h = proxy.health();
            if h.established >= 1
                && h.bytes_up >= payload.len() as u64
                && h.bytes_down >= payload.len() as u64
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let h = proxy.health();
        assert_eq!(h.established, 1, "one tunnel established");
        assert!(
            h.bytes_up >= payload.len() as u64,
            "counted up bytes: {h:?}"
        );
        assert!(
            h.bytes_down >= payload.len() as u64,
            "counted down bytes: {h:?}"
        );
        drop(echo_h);
    }

    #[test]
    fn dial_failure_returns_502() {
        let proxy = ConnectProxy::start("agent-y").expect("start");
        // 127.0.0.1:1 — nothing listens → dial fails → 502.
        let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).expect("dial proxy");
        s.write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("write");
        let mut resp = [0u8; 39];
        let n = s.read(&mut resp).expect("read");
        assert!(
            resp[..n].starts_with(b"HTTP/1.1 502"),
            "expected 502, got {:?}",
            String::from_utf8_lossy(&resp[..n])
        );
        for _ in 0..50 {
            if proxy.health().failed >= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(proxy.health().failed >= 1);
    }

    #[test]
    fn malformed_request_rejected_without_crash() {
        let proxy = ConnectProxy::start("agent-z").expect("start");
        let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).expect("dial proxy");
        s.write_all(b"GET / HTTP/1.1\r\n\r\n").expect("write");
        let mut resp = [0u8; 39];
        let n = s.read(&mut resp).unwrap_or(0);
        // 400 (or connection closed) — never a 200, never a panic.
        assert!(!resp[..n].starts_with(b"HTTP/1.1 200"));
        assert!(proxy.health().alive, "accept loop survives a bad request");
    }

    #[test]
    fn self_probe_true_for_live_loopback_target_false_for_dead() {
        let (echo_addr, echo_h) = spawn_echo();
        let proxy = ConnectProxy::start("agent-p").expect("start");
        assert!(
            proxy.self_probe(&echo_addr),
            "live loopback target → 200 → true"
        );
        assert!(
            !proxy.self_probe("127.0.0.1:1"),
            "dead target → 502 → false"
        );
        drop(echo_h);
    }

    #[test]
    fn reconcile_starts_missing_and_reaps_gone() {
        let mut proxies: HashMap<String, ConnectProxy> = HashMap::new();

        let live: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        reconcile(&mut proxies, &live);
        assert_eq!(proxies.len(), 2);
        assert!(proxies.contains_key("a") && proxies.contains_key("b"));
        let port_a = proxies["a"].port();
        assert!(port_a > 0);

        // "a" stays, "b" gone, "c" new.
        let live2: HashSet<String> = ["a", "c"].iter().map(|s| s.to_string()).collect();
        reconcile(&mut proxies, &live2);
        assert_eq!(proxies.len(), 2);
        assert!(proxies.contains_key("a") && proxies.contains_key("c"));
        assert!(!proxies.contains_key("b"), "gone agent's proxy reaped");
        assert_eq!(proxies["a"].port(), port_a, "surviving proxy not restarted");
    }

    #[test]
    fn health_beacon_accepts_loopback_connections() {
        let beacon = HealthBeacon::start().expect("beacon");
        assert!(beacon.port() > 0);
        // A raw TCP connect to the beacon must succeed (it's the self-probe target).
        assert!(TcpStream::connect(beacon.addr()).is_ok());
    }
}
