//! Daemon JSON control API over TCP loopback.
//!
//! Protocol: NDJSON (one JSON request per line, one JSON response per line).
//! Port is published to `{run_dir}/api.port`; see `ipc.rs` for the port
//! registry and loopback-binding rules.

use crate::agent::{AgentRegistry, ExternalRegistry};
use anyhow::Context;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub mod app_restart;
pub(crate) mod handlers;
mod operator_gate;
pub mod request_dedup;

pub type ConfigRegistry = Arc<Mutex<HashMap<String, crate::daemon::AgentConfig>>>;

pub(crate) fn list_response(
    home: &std::path::Path,
    registry: &crate::agent::AgentRegistry,
    externals: &crate::agent::ExternalRegistry,
) -> serde_json::Value {
    handlers::query::list_response(home, registry, externals)
}

// ---------------------------------------------------------------------------
// ApiNotifier — decouples api.rs from the TUI layer
// ---------------------------------------------------------------------------

/// Domain events emitted by the API server when agents or teams change.
/// These are independent of any UI representation.
#[derive(Debug)]
pub enum ApiEvent {
    InstanceCreated {
        name: String,
        layout: LayoutHint,
        spawner: Option<String>,
        target_pane: Option<String>,
    },
    InstanceDeleted {
        name: String,
    },
    TeamCreated {
        name: String,
        members: Vec<String>,
    },
    TeamMembersChanged {
        name: String,
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// A `move_pane` MCP call asked for the pane displaying `agent` to be
    /// relocated into `target_tab`. If the target tab exists the pane is
    /// grouped with it; otherwise a new tab with that name is created. Lets
    /// agents orchestrate team composition without the user dragging panes
    /// by hand — e.g. a supervisor adding a freshly-spawned reviewer into
    /// the existing team's tab.
    PaneMoved {
        agent: String,
        target_tab: String,
        split_dir: PaneMoveSplitDir,
    },
}

/// Direction to split the destination tab's focused pane when the target tab
/// already exists. Ignored when a new tab is created (the moved pane becomes
/// the tab's root either way).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaneMoveSplitDir {
    #[default]
    Horizontal,
    Vertical,
}

impl PaneMoveSplitDir {
    pub fn parse(s: &str) -> Self {
        match s {
            "vertical" | "v" => Self::Vertical,
            _ => Self::Horizontal,
        }
    }
}

/// Layout hint for newly created instances. Parsed at the API boundary so
/// invalid values are caught early rather than silently defaulting downstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LayoutHint {
    #[default]
    Tab,
    SplitRight,
    SplitBelow,
    /// #1431: place the new pane in the tab the same-named pane occupied
    /// before removal. Used by `restart_instance` so a restarted agent
    /// returns to its original tab instead of opening a fresh one.
    SameTab,
}

impl LayoutHint {
    pub fn parse(s: &str) -> Self {
        match s {
            "split-right" => Self::SplitRight,
            "split-below" => Self::SplitBelow,
            "same-tab" => Self::SameTab,
            _ => Self::Tab,
        }
    }
}

/// Trait for receiving API lifecycle notifications. Implementations decide
/// how (or whether) to react — the TUI adapter forwards to `TuiEvent`,
/// while daemon mode simply drops them.
pub trait ApiNotifier: Send + Sync {
    fn notify(&self, event: ApiEvent);
}

/// Validate a caller-supplied `working_directory` — rejects paths containing
/// `..` components. Sprint 29: canonicalize + allowed-roots removed per
/// over-engineering audit (daemon runs as user, full filesystem access).
pub fn validate_working_directory(
    path: &std::path::Path,
    home: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    use std::path::Component;
    // Reject path traversal at component level
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        anyhow::bail!("working_directory must not contain '..'");
    }
    // Canonicalize to resolve symlinks. `dunce::canonicalize` (not
    // `std::fs::canonicalize`) so that on Windows the returned path does NOT
    // carry the `\\?\` UNC verbatim prefix: this value becomes the PTY cwd
    // (agent::build_command -> cmd.cwd), and a `\\?\`-prefixed cwd makes
    // cmd.exe-based backends warn "UNC paths are not supported" and silently
    // fall back to C:\Windows (#893 — same prefix bug already fixed for the
    // session-name encode path in backend::canonicalize_for_encode).
    let canonical = if path.exists() {
        dunce::canonicalize(path)
            .map_err(|e| anyhow::anyhow!("working_directory canonicalize failed: {e}"))?
    } else {
        // Path doesn't exist yet (will be created) — use parent for validation
        path.to_path_buf()
    };
    // Allowed-roots check
    if !is_under_allowed_root(&canonical, home) {
        anyhow::bail!("working_directory outside allowed roots");
    }
    Ok(canonical)
}

/// Compute allowed root directories for working_directory validation.
fn allowed_roots(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut roots = vec![home.to_path_buf(), crate::paths::workspace_dir(home)];
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Ok(extra) = std::env::var("AGEND_ALLOWED_ROOTS") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for r in extra.split(sep) {
            if !r.is_empty() {
                roots.push(std::path::PathBuf::from(r));
            }
        }
    }
    roots
}

fn is_under_allowed_root(path: &std::path::Path, home: &std::path::Path) -> bool {
    let roots = allowed_roots(home);
    roots.iter().any(|root| {
        // Canonicalize root too (home might be a symlink). Use
        // `dunce::canonicalize` to match the prefix form of `path` above —
        // a `\\?\`-prefixed root vs a plain-prefixed path (or vice versa)
        // would make `starts_with` spuriously fail on Windows (#893).
        let canonical_root = dunce::canonicalize(root).unwrap_or_else(|_| root.clone());
        path.starts_with(&canonical_root)
    })
}

// Sprint 29: strip_verbatim_prefix removed — canonicalize no longer called.

/// API method name constants — single source of truth for the NDJSON protocol.
pub mod method {
    pub const LIST: &str = "list";
    pub const INJECT: &str = "inject";
    pub const KILL: &str = "kill";
    pub const DELETE: &str = "delete";
    pub const SPAWN: &str = "spawn";
    pub const SEND: &str = "send";
    pub const STATUS: &str = "status";
    pub const REGISTER_EXTERNAL: &str = "register_external";
    pub const DEREGISTER_EXTERNAL: &str = "deregister_external";
    pub const CREATE_TEAM: &str = "create_team";
    /// #hook-state-poc: lifecycle-hook event report from a backend hook
    /// command (`agend-terminal hook-event`). Shadow-mode only.
    pub const HOOK_EVENT: &str = "hook_event";
    pub const UPDATE_TEAM: &str = "update_team";
    pub const MOVE_PANE: &str = "move_pane";
    pub const SHUTDOWN: &str = "shutdown";
    /// #1339: operator-only mode control. A DIRECT method (not the `mcp_tool`
    /// tunnel) → the operator_gate treats it as the operator transport, so only
    /// the operator CLI can reach it; agents (mcp_tool-only) cannot.
    pub const MODE: &str = "mode";
    pub const SET_BLOCKED_REASON: &str = "set_blocked_reason";
    pub const CLEAR_BLOCKED_REASON: &str = "clear_blocked_reason";
    pub const MCP_TOOL: &str = "mcp_tool";
    pub const MCP_TOOLS_LIST: &str = "mcp_tools_list";
    pub const PANE_SNAPSHOT: &str = "pane_snapshot";
}

/// #2453 Stage R1: which host owns this API server, and therefore which restart
/// strategy `restart_daemon` dispatches to. Injected at [`serve`] from the
/// composition root (daemon / app / verify) — the explicit replacement for the
/// former implicit `RUN_CORE_ACTIVE` global proxy. Threaded through the API
/// [`handlers::HandlerCtx`] → MCP `RuntimeContext` so the restart handler
/// dispatches on an injected value, never a process-global (decision
/// d-20260712012329422433-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartCapability {
    /// Headless `run_core` daemon: owns the in-process self-respawn / legacy
    /// exit(42) machinery (the `RESTART_PENDING` consumer). Restart proceeds.
    Daemon,
    /// `agend-terminal app` (combined TUI+daemon, `run_app`): fail-closed in R1.
    /// The staged owner-restart strategy is a later slice (decision d-…2329422433-1).
    App,
    /// Any other API-server owner (e.g. `verify`) — default-deny. A DISTINCT
    /// value from `App` even though R1 routes both to a fail-closed response.
    Unsupported,
}

/// Start API socket server (blocks calling thread).
///
/// `notifier`: when running inside the TUI app, `Some(notifier)` to notify the
/// event loop about instance/team creation and deletion. Daemon mode passes
/// `None` and events are silently dropped.
///
/// `host`: the [`RestartCapability`] of this API-server owner, injected from the
/// composition root so `restart_daemon` dispatches to the owner's strategy.
// #2453 R2: the 8th arg (`app_restart`) crosses the clippy threshold; the args are
// the composition-root wiring (registry/config/notifier/host/restart channel) and
// bundling them into a struct would only move the arity elsewhere. Matches the
// existing allow on `handle_session`.
#[allow(clippy::too_many_arguments)]
pub fn serve(
    home: &Path,
    registry: AgentRegistry,
    shutdown: Arc<AtomicBool>,
    configs: ConfigRegistry,
    externals: ExternalRegistry,
    notifier: Option<Arc<dyn ApiNotifier>>,
    host: RestartCapability,
    app_restart: Option<crate::api::app_restart::AppRestart>,
) {
    // #945 Phase 0: time the bind+port-publish step directly (not the
    // spawn of api::serve thread — that's sub-ms). Operators care about
    // "when did api.port appear" for cold-start latency tracking.
    let _api_port_bind_start = std::time::Instant::now();
    let listener: TcpListener = match crate::ipc::bind_loopback() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "failed to bind API socket");
            return;
        }
    };
    let port = crate::ipc::local_port(&listener);
    let run_dir = crate::daemon::run_dir(home);
    if let Err(e) = crate::ipc::write_port(&run_dir, crate::ipc::API_NAME, port) {
        tracing::warn!(error = %e, "failed to publish API port");
        return;
    }
    tracing::info!(
        step = "api::serve::bind_and_publish_port",
        elapsed_ms = _api_port_bind_start.elapsed().as_millis() as u64,
        "bootstrap-step"
    );
    // P1-10: Load the per-daemon auth cookie (already issued by
    // `daemon::run` / `verify::run` before any server thread spawned). If
    // it's missing we fail closed — running without auth would be worse
    // than not serving.
    let cookie = match crate::auth_cookie::read_cookie(&run_dir) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "api.cookie missing; aborting serve");
            return;
        }
    };
    // P0a (#2342 B4): the operator full-capability token is minted alongside the
    // cookie by `auth_cookie::issue` (every boot path), so it is on disk before
    // this accept loop starts (publish-before-accept). Fail CLOSED if it is
    // missing: serving without the operator secret would either lock the operator
    // out (no full-cap principal possible) or force a default-allow fallback —
    // both worse than not serving.
    let operator_token = match crate::auth_cookie::read_operator_token(&run_dir) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "api.operator token missing; aborting serve");
            return;
        }
    };
    // P0a security-posture surface (dev2 A1 residual, task
    // t-20260709010037959088-61315-1): the operator token is 0600 in run_dir,
    // which isolates cross-USER only — a same-uid agent can read it. Until this
    // is `Resolved`, a Conversational responder that accepts inbound MUST NOT
    // ship (enforced by the `responder_inbound_requires_same_uid_isolation`
    // invariant). Surfaced here so the posture is visible in daemon logs.
    tracing::debug!(
        isolation = ?crate::auth_cookie::SAME_UID_OPERATOR_ISOLATION,
        "operator/agent same-uid secret-isolation status"
    );
    tracing::info!(port, "API listening");

    // #1189: write `.ready` in app (TUI) mode after confirmed bind success.
    // Daemon mode writes `.ready` later (after spawn loop) with richer semantics.
    if notifier.is_some() {
        let ready_path = run_dir.join(".ready");
        if let Err(e) = std::fs::write(&ready_path, chrono::Utc::now().to_rfc3339()) {
            tracing::warn!(path = %ready_path.display(), error = %e, "failed to write .ready marker");
        }
    }

    // #680: connection counter — limits concurrent API sessions.
    // Fixed const (#env-cleanup: was env-overridable via `AGEND_API_MAX_CONNS`;
    // demoted to YAGNI for single-user deploys).
    const API_MAX_CONNS: usize = 32;
    let max_conns: usize = API_MAX_CONNS;
    let active_conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // #941: signal listener entered the `accept()` blocking phase.
    // ThreadDumpHandler reads LISTENER_PHASE to surface H7 evidence
    // (whether the API listener is currently blocked on accept vs
    // actively dispatching a connection).
    LISTENER_PHASE.store(
        LISTENER_PHASE_IN_ACCEPT,
        std::sync::atomic::Ordering::Relaxed,
    );

    // #bughunt-r1 (#4): explicit accept-error handling. The old
    // `.incoming().flatten()` silently dropped every `accept()` Err with no log
    // and no backoff — a persistent failure (e.g. EMFILE) would hot-spin and the
    // operator would see nothing. Now: rate-limited log, brief backoff, and a
    // give-up after a sustained streak.
    let mut consecutive_accept_errors: u32 = 0;
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => {
                consecutive_accept_errors = 0;
                s
            }
            Err(e) => {
                consecutive_accept_errors += 1;
                let (should_log, should_break) =
                    accept_error_disposition(consecutive_accept_errors);
                if should_log {
                    tracing::warn!(
                        error = %e,
                        consecutive = consecutive_accept_errors,
                        "API accept() failed"
                    );
                }
                if should_break {
                    tracing::error!(
                        consecutive = consecutive_accept_errors,
                        "API accept() failing persistently — stopping accept loop"
                    );
                    break;
                }
                std::thread::sleep(ACCEPT_ERROR_BACKOFF);
                continue;
            }
        };
        // Phase flips to "processing" while we set up the per-session
        // thread; flips back to in_accept at top of next iteration.
        LISTENER_PHASE.store(
            LISTENER_PHASE_PROCESSING,
            std::sync::atomic::Ordering::Relaxed,
        );
        let _ = stream.set_nodelay(true);
        // #680: atomic reserve-then-check (no race between check and increment).
        // The slot is a RAII `ConnSlot` whose `Drop` releases it on EVERY exit
        // path (reject / spawn-fail / normal return / panic unwind) — bug-audit
        // Rank1: the old manual `fetch_sub` placed after `handle_session` leaked
        // the slot when that call panicked.
        let (conn_slot, prev) = ConnSlot::reserve(&active_conns);
        if prev >= max_conns {
            tracing::warn!("API connection rejected — at capacity");
            drop(stream);
            continue; // `conn_slot` drops here → reservation released
        }
        if prev >= max_conns * 3 / 4 {
            tracing::warn!(
                active = prev + 1,
                max = max_conns,
                "API connection pool nearing capacity"
            );
        }
        // Sprint 29: TCP read/write timeouts removed per operator directive
        // (m-41 #6 + m-102). Localhost-only daemon relies on PID watcher
        // (Sprint 25 P3 PR #263) + TCP EOF for dead-peer detection.
        let reg = Arc::clone(&registry);
        let home = home.to_path_buf();
        let shutdown = Arc::clone(&shutdown);
        let cfgs = Arc::clone(&configs);
        let ext = Arc::clone(&externals);
        let ntf = notifier.clone();
        // Cookie + operator token are `[u8; 32]` (Copy); each session gets its
        // own copies so the spawned closure satisfies `'static`.
        let session_cookie = cookie;
        let session_operator_token = operator_token;
        // #2453: `host` is a `Copy` enum; each session gets its own copy so the
        // spawned `move` closure satisfies `'static` (mirrors the cookie/token).
        let session_host = host;
        // #2453 R2: `AppRestart` is Clone (channel Sender + Arc gate), not Copy;
        // each session gets its own clone so the `move` closure satisfies `'static`.
        let session_app_restart = app_restart.clone();
        if std::thread::Builder::new()
            .name("api_handler".into())
            .spawn(move || {
                // Hold the reservation + session count for the whole session;
                // their `Drop` releases on normal return OR panic unwind.
                let _conn = conn_slot;
                let _census = crate::thread_census::register("api_handler");
                let _session = SessionCount::enter(&ACTIVE_API_SESSIONS);
                handle_session(
                    stream,
                    &reg,
                    &home,
                    &shutdown,
                    &cfgs,
                    &ext,
                    ntf.as_deref(),
                    session_operator_token,
                    session_cookie,
                    session_host,
                    session_app_restart,
                );
            })
            .is_err()
        {
            // Spawn failed: the closure (and the `conn_slot` it captured) is
            // dropped, releasing the reservation — no manual decrement needed.
            tracing::warn!("failed to spawn API handler thread");
        }
        // Back to accept-blocking phase before the next iteration's
        // blocking incoming().next().
        LISTENER_PHASE.store(
            LISTENER_PHASE_IN_ACCEPT,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

// ── #941: thread-dump observability surface (Dim 4) ────────────────────
//
// Two atomics exposed for `daemon::per_tick::thread_dump::ThreadDumpHandler`:
//
// - LISTENER_PHASE: which phase the API listener thread is in
//   (0 = processing a connection, 1 = blocked in accept()). Helps
//   diagnose H7 (signal handler thread starved by long-running blocking
//   work) by showing whether the listener is in the expected resting
//   state during a wedge.
// - ACTIVE_API_SESSIONS: count of in-flight `handle_session` threads.
//   Surrogate for "how many concurrent API requests are being processed";
//   pairs with the registry-holder + handler-timing dimensions for a
//   complete daemon-thread snapshot.
//
// Both use `Ordering::Relaxed` because exact serialization across cores
// isn't needed for periodic dump observability (dump is wall-clock
// sampled, not transaction-ordered). The counters are monotonically
// incremented/decremented on the same thread per session, so no
// inter-thread ordering matters for individual values.

pub const LISTENER_PHASE_PROCESSING: u8 = 0;
pub const LISTENER_PHASE_IN_ACCEPT: u8 = 1;

/// #bughunt-r1 (#4): backoff slept after each `accept()` error so a persistent
/// failure (e.g. EMFILE from fd exhaustion) doesn't hot-spin the CPU.
const ACCEPT_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);
/// Log only the 1st error in a streak and every Nth thereafter (rate-limit).
const ACCEPT_ERROR_LOG_EVERY: u32 = 50;
/// Give up the accept loop after this many consecutive `accept()` errors — the
/// listener is wedged (not a transient blip), so spinning forever helps nobody.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 100;

/// #bughunt-r1 (#4): decide how the accept loop reacts to the Nth consecutive
/// `accept()` error — `(should_log, should_break)`. Pure so it is unit-testable
/// without inducing real socket errors. `consecutive` is 1-based (the first
/// error in a streak is 1).
fn accept_error_disposition(consecutive: u32) -> (bool, bool) {
    let should_log = consecutive == 1 || consecutive.is_multiple_of(ACCEPT_ERROR_LOG_EVERY);
    let should_break = consecutive >= MAX_CONSECUTIVE_ACCEPT_ERRORS;
    (should_log, should_break)
}

/// Current API listener thread phase. Read by the periodic thread-dump
/// handler. Zero (`LISTENER_PHASE_PROCESSING`) on initial daemon boot
/// before `serve` runs; set to `LISTENER_PHASE_IN_ACCEPT` immediately
/// before `listener.incoming()`.
pub static LISTENER_PHASE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(LISTENER_PHASE_PROCESSING);

/// Active API session count (per-connection `handle_session` threads
/// in flight). Read by the periodic thread-dump handler.
pub static ACTIVE_API_SESSIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// RAII reservation of an `active_conns` slot. `reserve` performs the #680
/// atomic `fetch_add` (reserve-then-check) and returns the prior count for the
/// capacity gate; `Drop` performs the matching `fetch_sub`. The guard is held
/// for the connection's whole lifetime (moved into the handler thread), so the
/// slot is released on EVERY exit path — normal return, an over-capacity reject,
/// a thread-spawn failure (the closure and its captured guard are dropped), and
/// crucially a `handle_session` PANIC unwind. A previous bug released the slot
/// with a manual `fetch_sub` placed AFTER `handle_session`, which the unwind
/// skipped — leaked slots accumulated to `API_MAX_CONNS` and locked out the
/// control plane (bug-audit Rank1).
struct ConnSlot {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl ConnSlot {
    /// Reserve a slot (atomic `fetch_add`); returns the guard + the prior count.
    fn reserve(counter: &Arc<std::sync::atomic::AtomicUsize>) -> (Self, usize) {
        let prev = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (
            Self {
                counter: Arc::clone(counter),
            },
            prev,
        )
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// RAII counter for in-flight `handle_session` threads (the #941
/// `ACTIVE_API_SESSIONS` observability surrogate): `enter` does `fetch_add`,
/// `Drop` does `fetch_sub`, so a `handle_session` panic unwind decrements it
/// instead of leaking the count.
struct SessionCount(&'static std::sync::atomic::AtomicUsize);

impl SessionCount {
    fn enter(counter: &'static std::sync::atomic::AtomicUsize) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for SessionCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_session(
    stream: TcpStream,
    registry: &AgentRegistry,
    home: &Path,
    shutdown: &Arc<AtomicBool>,
    configs: &ConfigRegistry,
    externals: &ExternalRegistry,
    notifier: Option<&dyn ApiNotifier>,
    operator_token: crate::auth_cookie::Cookie,
    cookie: crate::auth_cookie::Cookie,
    host: RestartCapability,
    app_restart: Option<crate::api::app_restart::AppRestart>,
) {
    let cloned = match stream.try_clone() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "API stream clone failed");
            return;
        }
    };
    let mut reader = BufReader::new(cloned);
    let mut writer = stream;

    // P1-10 gate: first NDJSON line must be `{"auth":"<hex>"}`. Read deadline
    // on the stream (set in `serve`) ensures a silent peer closes out in 30s
    // rather than pinning this worker thread.
    // #680: 5s pre-auth timeout — prevents slow-loris holding a semaphore slot.
    // CR-2026-06-14: arm it on the fd that the handshake actually READS from
    // (`reader`'s inner `cloned` stream), not on `writer`. They only happened to
    // share `SO_RCVTIMEO` because `try_clone()`/`dup` shares one file
    // description — an accidental coupling that any future independent-timeout
    // change (or a platform where dup'd handles don't share the option) would
    // silently break, removing the slow-loris guard.
    let _ = reader
        .get_ref()
        .set_read_timeout(Some(std::time::Duration::from_secs(5)));
    // P0a (#2342 B4): the handshake now also resolves WHICH principal
    // authenticated (operator full-cap token vs shared agent cookie) — authority
    // is proven by this principal, not by method-shape (dev2 A1). Sprint 25 P1 F1:
    // the optional peer PID (telemetry) is still returned alongside.
    let (principal, peer_pid) = match crate::auth_cookie::server_handshake_ndjson(
        &mut reader,
        &mut writer,
        &operator_token,
        &cookie,
    ) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(error = %e, "API auth rejected");
            return;
        }
    };
    // Restore no-timeout for authenticated sessions (on the read fd it was
    // armed on above — CR-2026-06-14).
    let _ = reader.get_ref().set_read_timeout(None);
    // Telemetry only — see MCP-DAEMON-PROXY-CONTRACT §peer-PID-telemetry.
    if let Some(pid) = peer_pid {
        tracing::debug!(peer_pid = pid, "API session peer PID");
    }

    // Sprint 29: all TCP timeouts removed per operator directive.
    // PID watcher handles dead-peer detection; TCP EOF handles clean close.

    // Sprint 25 P3: active peer PID watch — the real liveness check
    // (~2 s detection) for bridge sessions.
    if let Some(pid) = peer_pid {
        if let Ok(shutdown_stream) = writer.try_clone() {
            spawn_peer_pid_watcher(pid, shutdown_stream);
        }
    }

    loop {
        // Sprint 60 W1 PR-3 (#P0-3): the `restart_daemon` MCP handler
        // sets `RESTART_PENDING` (process-wide static) since MCP
        // handlers don't carry the shutdown flag in HandlerCtx. Bridge
        // it here so the main daemon loop notices and breaks.
        if crate::daemon::RESTART_PENDING.load(std::sync::atomic::Ordering::Acquire) {
            shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
            break;
        }
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let _ = writeln!(
                    writer,
                    "{}",
                    json!({"ok": false, "error": format!("parse: {e}")})
                );
                continue;
            }
        };

        let method = req["method"].as_str().unwrap_or("");
        let params = &req["params"];
        // #842: bridge-emitted `request_id` (UUIDv4) drives idempotent
        // retry. Missing → skip dedup (legacy at-least-once for clients
        // that don't emit the field; see `request_dedup::DedupCache::dispatch`).
        let request_id = req["request_id"].as_str();

        // #2453 R2 flush barrier: a fresh per-request slot. `restart_daemon` (if this
        // request is one) registers a commit-permission ack into it; we run that ack
        // AFTER writing+flushing this response (see below), so the app tears down and
        // re-execs only once its `prepared` reply is on the socket.
        let post_flush = crate::api::app_restart::PostFlushSlot::new();
        let ctx = handlers::HandlerCtx {
            registry,
            configs,
            externals,
            notifier,
            home,
            capability: host,
            app_restart: app_restart.as_ref(),
            post_flush: post_flush.clone(),
        };

        // P0a (#2342 B4): per-method CAPABILITY gate FIRST — authority is proven
        // by the authenticated principal (which secret was presented), not by
        // method-shape. Closes the method-shape / sidecar-agent-cookie subcase of
        // dev2 A1 (the same-user-agent subcase — a same-uid agent reading
        // `api.operator` — is a HARD Phase-2 prereq: `auth_cookie::SAME_UID_OPERATOR_ISOLATION`).
        // HARD default-DENY: an Agent-cookie holder reaching any direct method
        // (inject/send/spawn/kill/…) is refused here, before the operator-mode gate.
        // Distinct `denied_by` ("capability", not queued) keeps the denial legible in audit.
        //
        // #1339: the operator-mode authority gate then covers the `mcp_tool`
        // tunnel (per-tool authority) at the one ingress choke point. By here an
        // Agent principal can only be on `mcp_tool`/`mcp_tools_list`; Operator has
        // full authority (mode gate is a pass-through for direct methods). A deny
        // short-circuits before dispatch.
        let response = if !operator_gate::capability_allows_request(principal, method, params) {
            json!({
                "ok": false,
                "error": format!(
                    "method '{method}' is not permitted for this connection's token \
                     (capability gate: default-deny)"
                ),
                "denied_by": "capability"
            })
        } else if let Err(denied) =
            operator_gate::check_operation_allowed(method, params, &crate::operator_mode::get())
        {
            json!({"ok": false, "error": denied, "denied_by": "operator_mode", "queued": true})
        } else {
            request_dedup::global().dispatch(
                request_id,
                request_dedup::operation_fingerprint(method, params),
                request_dedup::method_wait_timeout(method, params),
                || match method {
                    method::LIST => handlers::query::handle_list(params, &ctx),
                    method::INJECT => handlers::instance::handle_inject(params, &ctx),
                    method::KILL => handlers::instance::handle_kill(params, &ctx),
                    method::DELETE => handlers::instance::handle_delete(params, &ctx),
                    method::SPAWN => handlers::instance::handle_spawn(params, &ctx),
                    method::SEND => handlers::messaging::handle_send(params, &ctx),
                    method::STATUS => handlers::query::handle_status(params, &ctx),
                    method::REGISTER_EXTERNAL => {
                        handlers::external::handle_register_external(params, &ctx)
                    }
                    method::DEREGISTER_EXTERNAL => {
                        handlers::external::handle_deregister_external(params, &ctx)
                    }
                    method::CREATE_TEAM => handlers::team::handle_create_team(params, &ctx),
                    method::HOOK_EVENT => handlers::hook_event::handle_hook_event(params, &ctx),
                    method::UPDATE_TEAM => handlers::team::handle_update_team(params, &ctx),
                    method::MOVE_PANE => handlers::instance::handle_move_pane(params, &ctx),
                    method::PANE_SNAPSHOT => handlers::instance::handle_pane_snapshot(params, &ctx),
                    method::SET_BLOCKED_REASON => {
                        handlers::instance::handle_set_blocked_reason(params, &ctx)
                    }
                    method::CLEAR_BLOCKED_REASON => {
                        handlers::instance::handle_clear_blocked_reason(params, &ctx)
                    }
                    method::MCP_TOOL => handlers::mcp_proxy::handle_mcp_tool(params, &ctx),
                    method::MCP_TOOLS_LIST => {
                        handlers::mcp_proxy::handle_mcp_tools_list(params, &ctx)
                    }
                    // #1339: operator-only mode control (operator transport).
                    method::MODE => operator_gate::handle_mode_set(params, home),
                    method::SHUTDOWN => {
                        tracing::info!("API shutdown requested");
                        // Sprint 57 Wave 3 PR-2 (#548 Q6): record API-shutdown
                        // reason BEFORE flipping the flag so the shutdown
                        // sequence sees the right taxonomy when it reads.
                        crate::daemon::record_shutdown_reason(
                            crate::daemon::ShutdownReason::ApiShutdown,
                        );
                        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
                        json!({"ok": true})
                    }
                    _ => json!({"ok": false, "error": format!("unknown method: {method}")}),
                },
            )
        };

        // #2453 R2 P0: a NON-CACHEABLE app-restart response (the handler marked it, or it
        // armed the post-flush slot) reflects momentary AppRestartGate state — prepared /
        // retryable in_progress loser / aborted / timed-out. The gate, NOT this cache, is
        // the idempotence authority, so evict its request_id: a same-id retry must re-enter
        // the handler and be judged against CURRENT gate state, never served a stale
        // transient (e.g. a cached in_progress after the winner aborted → the retry would
        // never re-enter the now-Serving gate). Ordinary responses stay cached as before.
        maybe_evict_noncacheable_restart(request_id, &post_flush, request_dedup::global());
        // #2453 R2 flush barrier: write+flush THIS response, then run any registered
        // post-flush action with whether BOTH succeeded. On success the restart ack
        // fires (→ the TUI commits + re-execs); on any failure the action is dropped
        // un-run → the TUI's `flush_ack` disconnects → it aborts (gate back to
        // Serving). If the session loop exits before reaching here, `post_flush`
        // drops with an un-run action → same disconnect → abort.
        let wrote = writeln!(writer, "{}", response).is_ok();
        let flushed = wrote && writer.flush().is_ok();
        post_flush.run_after_flush(flushed);
    }
}

/// #2453 R2 P0: evict the dedup entry for a NON-CACHEABLE app-restart response. Every
/// gate-dependent response the app-restart handler produces — `prepared` (armed),
/// retryable `in_progress` (CAS loser), `aborted`, `timed-out` — reflects momentary
/// [`crate::api::app_restart::AppRestartGate`] state, and the gate (not this cache) is
/// the idempotence authority. Caching any of them would let a later same-id retry observe
/// a stale transient — e.g. a cached `in_progress` after the winner aborted would wedge
/// retry-after-abort, never re-entering the now-`Serving` gate. Evicting makes every
/// same-id restart_daemon call re-enter the handler → the gate judges from CURRENT state.
/// Ordinary responses (slot neither marked nor armed) stay cached. Cross-platform (Windows
/// fail-closes at the handler → the slot is never marked/armed there → no-op).
fn maybe_evict_noncacheable_restart(
    request_id: Option<&str>,
    post_flush: &crate::api::app_restart::PostFlushSlot,
    cache: &request_dedup::DedupCache,
) {
    if post_flush.is_non_cacheable() {
        if let Some(id) = request_id.filter(|s| !s.is_empty()) {
            cache.evict(id);
        }
    }
}

// ---------------------------------------------------------------------------
// Active peer PID watch (Sprint 25 P3)
// ---------------------------------------------------------------------------

/// Check if a process is alive via `kill(pid, 0)` (Unix) or
/// `OpenProcess` (Windows).
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) sends no signal; it only checks if the
    // process exists and we have permission to signal it. ESRCH = dead.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn is_process_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.process(Pid::from_u32(pid)).is_some()
}

/// Spawn a background thread that polls peer PID liveness every ~2s.
/// When the peer dies, shuts down the TCP stream so the session's
/// `read_line` returns EOF immediately instead of waiting for the 30s
/// TCP read timeout.
///
/// // fire-and-forget: watcher self-terminates when peer dies or stream
/// // is already closed. No JoinHandle join needed — session thread
/// // exits independently and the watcher's stream shutdown is idempotent.
fn spawn_peer_pid_watcher(pid: u32, stream: std::net::TcpStream) {
    // fire-and-forget: PID watcher polls until peer dies then self-exits.
    // Stream shutdown is idempotent; if session already closed, shutdown
    // returns an error that we silently ignore.
    std::thread::Builder::new()
        .name(format!("pid_watch_{pid}"))
        .spawn(move || {
            let _census = crate::thread_census::register("pid_watcher");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if !is_process_alive(pid) {
                    tracing::info!(peer_pid = pid, "peer process dead — closing API session");
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    return;
                }
            }
        })
        .ok();
}

/// Send a request to the daemon API and read one NDJSON response.
///
/// Performs the P1-10 cookie handshake first: reads `api.cookie` from the
/// active daemon's run dir, sends `{"auth":"<hex>"}`, and rejects the call
/// if the server does not reply `{"ok":true}`. The cookie file has mode
/// 0600 so only the daemon's user can read it — this is the peer-UID
/// substitute for TCP loopback (see `auth_cookie.rs`).
/// #1814: like [`call`] but targets a SPECIFIC run dir (cookie + api.port read
/// from `run_dir`), not the active daemon. Used by the self-respawn Phase-1
/// gate so the predecessor can do a real cookie-authenticated round-trip
/// against the successor's own control plane while both are briefly alive. A
/// short fixed read timeout (the gate retries) keeps a flaky successor from
/// hanging the handler. No self-IPC guard: this connects to a DIFFERENT
/// process's socket, so the registry-lock deadlock class does not apply.
pub fn call_at(run_dir: &Path, request: &Value) -> anyhow::Result<Value> {
    let stream = crate::ipc::connect_run_dir_api(run_dir)?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .context("set call_at read timeout")?;
    // P0a (#2342 B4): the operator surface presents the operator full-capability
    // token, NOT the shared agent cookie — so it authenticates as
    // `Principal::Operator` (allow-all). Fail CLOSED if the token is missing
    // (`?`): never silently fall back to the cookie, which would authenticate as
    // a mere Agent and get every direct method denied (operator lockout).
    let operator_token = crate::auth_cookie::read_operator_token(run_dir)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, &operator_token)?;
    writeln!(writer, "{request}")?;
    writer.flush()?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

pub fn call(home: &Path, request: &Value) -> anyhow::Result<Value> {
    // #1492: self-IPC over the loopback socket. If the caller holds the
    // registry lock, the API handler servicing this call needs the same lock →
    // deadlock. #1492-L2: the guard is always-on and fail-fast — on a violation
    // it logs + returns `Err` here (in EVERY build, not just debug), so the
    // deadlocking call is refused and the daemon stays live instead of freezing.
    crate::sync_audit::assert_no_registry_lock_for_self_ipc("api::call")?;
    // #bughunt-r1 (#2 TOCTOU): resolve the active run dir ONCE and read BOTH the
    // api port and the cookie from it. The previous code connected via
    // `connect_api` (which resolved the run dir internally for the port) and THEN
    // re-resolved via `find_active_run_dir` for the cookie — during a daemon
    // restart those two resolutions could land on DIFFERENT run dirs (run dir B's
    // cookie sent to run dir A's socket → handshake failure). Mirror `call_at`.
    let run = crate::daemon::find_active_run_dir(home)
        .ok_or_else(|| anyhow::anyhow!("no active daemon (run dir not found)"))?;
    let stream = crate::ipc::connect_run_dir_api(&run)?;
    // #1492 backstop (L3): bound every loopback read with a socket-level
    // timeout. #1492-L2 made the guard above always-on + fail-fast (it now
    // returns `Err` in every build, not just a debug panic), so a self-IPC made
    // while holding the registry/core lock is refused before we ever reach this
    // read. This timeout is the complementary containment net (defense-in-depth)
    // for any future path that bypasses the guard: were such a read to block
    // forever in `recvfrom` while holding the lock, it would freeze the whole
    // TUI permanently. The timeout converts that into a recoverable error: the
    // read fails, this call unwinds, the offending lock guard drops, and the
    // waiting threads proceed.
    // Generous fixed timeout (covers the slowest legit method, create_instance ~60s).
    let timeout = api_call_read_timeout();
    stream
        .set_read_timeout(Some(timeout))
        .context("set api::call read timeout")?;
    // P0a (#2342 B4): present the operator full-capability token (NOT the shared
    // agent cookie) so this operator-surface call authenticates as
    // `Principal::Operator` = allow-all. Read from the SAME `run` resolution used
    // for the port above (#2 fix). Fail CLOSED if missing (`?`) — never fall back
    // to the cookie (that would authenticate as Agent → direct methods denied →
    // operator locked out of its own daemon).
    let operator_token = crate::auth_cookie::read_operator_token(&run)?;

    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, &operator_token)?;

    writeln!(writer, "{}", request)?;
    writer.flush()?;

    let mut line = String::new();
    if let Err(e) = reader.read_line(&mut line) {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            let method = request["method"].as_str().unwrap_or("<unknown>");
            tracing::warn!(
                method,
                ?timeout,
                "#1492: api::call read timed out — the loopback handler did not respond \
                 in time. This is the self-IPC-deadlock backstop firing; the most likely \
                 cause is a caller that invoked api::call while holding the registry/core \
                 lock (drop the guard BEFORE the call — see docs/DAEMON-LOCK-ORDERING.md)."
            );
            anyhow::bail!("api::call ({method}) timed out after {timeout:?}");
        }
        return Err(e).context("api::call read response");
    }
    let resp: Value = serde_json::from_str(line.trim())?;
    Ok(resp)
}

/// Read-response timeout for [`call`]. Defaults to 90s — comfortably above the
/// slowest legitimate daemon method (`create_instance`, whose own budget is
/// ~60s) so a genuine slow call never trips the backstop, while still bounding a
/// wedged self-IPC instead of blocking forever. Overridable via
/// Fixed const 90s (#env-cleanup: was env-overridable via
/// `AGEND_API_CALL_TIMEOUT_SECS`; demoted to YAGNI for single-user deploys).
fn api_call_read_timeout() -> std::time::Duration {
    const API_CALL_READ_TIMEOUT_SECS: u64 = 90;
    std::time::Duration::from_secs(API_CALL_READ_TIMEOUT_SECS)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// #2453 R2 P0 seam: `maybe_evict_noncacheable_restart` — the handle_session
    /// post-flush step — evicts the request_id when the post-flush slot is either
    /// ARMED (a `prepared` response) OR MARKED non-cacheable (a transient loser /
    /// abort / timeout), and leaves an ordinary (neither) response cached.
    #[test]
    fn maybe_evict_noncacheable_restart_evicts_armed_and_marked_only() {
        use crate::api::app_restart::PostFlushSlot;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let fp =
            request_dedup::operation_fingerprint("mcp_tool", &json!({"tool": "restart_daemon"}));

        // Assert a retry re-runs the handler after `slot` (set up by `arm`) is evicted.
        fn evict_then_retry_reruns(fp: u64, id: &str, arm: impl FnOnce(&PostFlushSlot)) -> bool {
            let cache = request_dedup::DedupCache::default();
            cache.dispatch(
                Some(id),
                fp,
                std::time::Duration::from_secs(5),
                || json!({"ok": true, "restart": "prepared"}),
            );
            let slot = PostFlushSlot::new();
            arm(&slot);
            maybe_evict_noncacheable_restart(Some(id), &slot, &cache);
            let reran = Arc::new(AtomicBool::new(false));
            let rr = Arc::clone(&reran);
            cache.dispatch(Some(id), fp, std::time::Duration::from_secs(5), move || {
                rr.store(true, Ordering::SeqCst);
                json!({"ok": false})
            });
            reran.load(Ordering::SeqCst)
        }

        // ARMED (prepared) ⇒ evicted → retry re-runs.
        assert!(
            evict_then_retry_reruns(fp, "armed", |s| {
                assert!(s.register(Box::new(|| {})));
            }),
            "an armed (prepared) response must be evicted so the retry re-runs"
        );
        // MARKED non-cacheable (transient loser/abort/timeout, NOT armed) ⇒ evicted too.
        assert!(
            evict_then_retry_reruns(fp, "marked", |s| s.mark_non_cacheable()),
            "a marked (transient) restart response must be evicted so the retry re-runs"
        );

        // Neither armed nor marked ⇒ an ordinary response stays cached (retry must NOT re-run).
        let cache2 = request_dedup::DedupCache::default();
        cache2.dispatch(
            Some("plain"),
            fp,
            std::time::Duration::from_secs(5),
            || json!({"ok": true, "n": 1}),
        );
        let unarmed = PostFlushSlot::new();
        maybe_evict_noncacheable_restart(Some("plain"), &unarmed, &cache2);
        let reran2 = Arc::new(AtomicBool::new(false));
        let rr2 = Arc::clone(&reran2);
        let cached = cache2.dispatch(
            Some("plain"),
            fp,
            std::time::Duration::from_secs(5),
            move || {
                rr2.store(true, Ordering::SeqCst);
                json!({"ok": true, "n": 2})
            },
        );
        assert!(
            !reran2.load(Ordering::SeqCst),
            "an ordinary (unarmed) response must stay cached — the retry must NOT re-run"
        );
        assert_eq!(cached["n"], 1, "the retry observed the cached response");
    }

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-api-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn validate_work_dir_rejects_parent_dir() {
        let home = tmp_home("validate_parent");
        let bad = home.join("..").join("escape");
        let err = validate_working_directory(&bad, &home).unwrap_err();
        assert!(
            format!("{err}").contains(".."),
            "expected parent-dir rejection, got: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn validate_work_dir_allows_normal_path() {
        let home = tmp_home("validate_normal");
        let ok = crate::paths::workspace_dir(&home).join("agent");
        std::fs::create_dir_all(&ok).expect("create dir");
        let resolved = validate_working_directory(&ok, &home).expect("normal path must validate");
        assert!(resolved.ends_with("agent"));
        std::fs::remove_dir_all(&home).ok();
    }

    /// Windows-only #893 regression: the path returned by
    /// `validate_working_directory` becomes the PTY cwd
    /// (`agent::build_command` -> `cmd.cwd`). It MUST NOT carry the `\\?\`
    /// UNC verbatim prefix that `std::fs::canonicalize` returns on Windows —
    /// a `\\?\`-prefixed cwd makes cmd.exe-based backends warn "UNC paths are
    /// not supported" and fall back to C:\Windows. `dunce::canonicalize`
    /// strips the prefix when safe; falling back to `std::fs::canonicalize`
    /// here would re-introduce the bug. The validation must also still SUCCEED
    /// (exercises `is_under_allowed_root`, which must canonicalize the root the
    /// same way or the `starts_with` check would spuriously reject).
    #[cfg(windows)]
    #[test]
    fn validate_work_dir_strips_verbatim_prefix_on_windows() {
        let home = tmp_home("validate_verbatim");
        let work = crate::paths::workspace_dir(&home).join("agent");
        std::fs::create_dir_all(&work).expect("create dir");
        let resolved = validate_working_directory(&work, &home)
            .expect("existing path under home must validate");
        let resolved_str = resolved.to_string_lossy();
        assert!(
            !resolved_str.starts_with(r"\\?\"),
            "validate_working_directory must strip the Windows `\\\\?\\` verbatim \
             prefix (got {resolved_str:?}); this path becomes the PTY cwd and a \
             `\\\\?\\` cwd breaks cmd.exe-based backends — keep dunce::canonicalize, \
             do not fall back to std::fs::canonicalize"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn validate_work_dir_rejects_outside_roots() {
        let home = tmp_home("validate_outside");
        // /tmp exists but is not under home or workspace
        let outside = std::path::PathBuf::from("/tmp");
        let err = validate_working_directory(&outside, &home).unwrap_err();
        assert!(
            format!("{err}").contains("outside allowed roots"),
            "expected roots rejection, got: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn validate_work_dir_env_override_accepted() {
        let home = tmp_home("validate_env");
        // Use a sibling dir of home (not under home) as custom root
        let custom = home.parent().unwrap().join("agend-custom-root-707");
        std::fs::create_dir_all(&custom).expect("create custom");
        let sep = if cfg!(windows) { ";" } else { ":" };
        let canonical_custom = std::fs::canonicalize(&custom).unwrap_or_else(|_| custom.clone());
        std::env::set_var(
            "AGEND_ALLOWED_ROOTS",
            canonical_custom.to_str().unwrap_or(""),
        );
        let result = validate_working_directory(&custom, &home);
        std::env::remove_var("AGEND_ALLOWED_ROOTS");
        assert!(result.is_ok(), "env override should allow: {result:?}");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&custom).ok();
        let _ = sep; // suppress unused warning
    }

    #[test]
    fn call_fails_without_daemon() {
        let home = tmp_home("call_no_daemon");
        let err = call(&home, &json!({"method": "list"})).unwrap_err();
        // No active daemon → either "no active daemon" or a TCP ConnectionRefused
        let msg = format!("{err:#}");
        assert!(
            msg.to_ascii_lowercase().contains("no active daemon")
                || msg.to_ascii_lowercase().contains("refused"),
            "unexpected error: {msg}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn api_call_read_timeout_is_fixed_90s() {
        // #1492 backstop: the loopback read timeout that converts a wedged
        // self-IPC from a permanent daemon freeze into a recoverable error.
        // #env-cleanup: now a fixed const (the AGEND_API_CALL_TIMEOUT_SECS
        // override + sub-1s clamp were demoted). 90s must still exceed the
        // slowest legit method (create_instance ~60s).
        assert_eq!(
            api_call_read_timeout(),
            std::time::Duration::from_secs(90),
            "fixed default must exceed the slowest legit method (create_instance ~60s)"
        );
    }

    // -----------------------------------------------------------------------
    // ApiNotifier seam tests
    // -----------------------------------------------------------------------

    /// Test-only notifier that records every event for later assertion.
    struct RecordingNotifier {
        events: parking_lot::Mutex<Vec<ApiEvent>>,
    }

    impl RecordingNotifier {
        fn new() -> Self {
            Self {
                events: parking_lot::Mutex::new(Vec::new()),
            }
        }
        fn take(&self) -> Vec<ApiEvent> {
            std::mem::take(&mut *self.events.lock())
        }
    }

    impl ApiNotifier for RecordingNotifier {
        fn notify(&self, event: ApiEvent) {
            self.events.lock().push(event);
        }
    }

    // -- Positive: 4 call-site tests (full payload assertion) --

    #[test]
    fn notifier_receives_instance_deleted() {
        let rec = RecordingNotifier::new();
        rec.notify(ApiEvent::InstanceDeleted {
            name: "agent-1".into(),
        });
        let events = rec.take();
        assert_eq!(events.len(), 1);
        let ApiEvent::InstanceDeleted { name } = &events[0] else {
            panic!("wrong variant")
        };
        assert_eq!(name, "agent-1");
    }

    #[test]
    fn notifier_receives_instance_created() {
        let rec = RecordingNotifier::new();
        rec.notify(ApiEvent::InstanceCreated {
            name: "agent-2".into(),
            layout: LayoutHint::SplitRight,
            spawner: Some("caller".into()),
            target_pane: None,
        });
        let events = rec.take();
        assert_eq!(events.len(), 1);
        let ApiEvent::InstanceCreated {
            name,
            layout,
            spawner,
            target_pane,
        } = &events[0]
        else {
            panic!("wrong variant")
        };
        assert_eq!(name, "agent-2");
        assert_eq!(*layout, LayoutHint::SplitRight);
        assert_eq!(spawner.as_deref(), Some("caller"));
        assert_eq!(*target_pane, None);
    }

    #[test]
    fn notifier_receives_team_created() {
        let rec = RecordingNotifier::new();
        rec.notify(ApiEvent::TeamCreated {
            name: "team-a".into(),
            members: vec!["m1".into(), "m2".into()],
        });
        let events = rec.take();
        assert_eq!(events.len(), 1);
        let ApiEvent::TeamCreated { name, members } = &events[0] else {
            panic!("wrong variant")
        };
        assert_eq!(name, "team-a");
        assert_eq!(members, &["m1", "m2"]);
    }

    #[test]
    fn notifier_receives_team_members_changed() {
        let rec = RecordingNotifier::new();
        rec.notify(ApiEvent::TeamMembersChanged {
            name: "team-b".into(),
            added: vec!["new".into()],
            removed: vec!["old".into()],
        });
        let events = rec.take();
        assert_eq!(events.len(), 1);
        let ApiEvent::TeamMembersChanged {
            name,
            added,
            removed,
        } = &events[0]
        else {
            panic!("wrong variant")
        };
        assert_eq!(name, "team-b");
        assert_eq!(added, &["new"]);
        assert_eq!(removed, &["old"]);
    }

    // -- None-path: 4 tests verifying no panic when notifier is None --

    #[test]
    fn none_notifier_instance_deleted_no_panic() {
        let notifier: Option<&dyn ApiNotifier> = None;
        if let Some(n) = notifier {
            n.notify(ApiEvent::InstanceDeleted { name: "x".into() });
        }
    }

    #[test]
    fn none_notifier_instance_created_no_panic() {
        let notifier: Option<&dyn ApiNotifier> = None;
        if let Some(n) = notifier {
            n.notify(ApiEvent::InstanceCreated {
                name: "x".into(),
                layout: LayoutHint::Tab,
                spawner: None,
                target_pane: None,
            });
        }
    }

    #[test]
    fn none_notifier_team_created_no_panic() {
        let notifier: Option<&dyn ApiNotifier> = None;
        if let Some(n) = notifier {
            n.notify(ApiEvent::TeamCreated {
                name: "x".into(),
                members: vec![],
            });
        }
    }

    #[test]
    fn none_notifier_team_members_changed_no_panic() {
        let notifier: Option<&dyn ApiNotifier> = None;
        if let Some(n) = notifier {
            n.notify(ApiEvent::TeamMembersChanged {
                name: "x".into(),
                added: vec![],
                removed: vec![],
            });
        }
    }

    // -- Failure resilience --

    /// A notifier that panics on every call — used to verify that a panicking
    /// notifier does not silently corrupt state in the RecordingNotifier path.
    /// Note: in production, a panic inside `notify()` will unwind through
    /// `handle_session`, terminating that API connection. This is acceptable
    /// because notifier implementations (TuiNotifier) never panic.
    struct PanickingNotifier;

    impl ApiNotifier for PanickingNotifier {
        fn notify(&self, _event: ApiEvent) {
            panic!("intentional test panic");
        }
    }

    #[test]
    fn panicking_notifier_unwinds_safely() {
        let result = std::panic::catch_unwind(|| {
            let n: &dyn ApiNotifier = &PanickingNotifier;
            n.notify(ApiEvent::InstanceDeleted { name: "x".into() });
        });
        assert!(result.is_err(), "expected panic to propagate");
    }

    #[test]
    fn notifier_multiple_events_accumulate() {
        let rec = RecordingNotifier::new();
        rec.notify(ApiEvent::InstanceCreated {
            name: "a".into(),
            layout: LayoutHint::Tab,
            spawner: None,
            target_pane: None,
        });
        rec.notify(ApiEvent::InstanceDeleted { name: "a".into() });
        let events = rec.take();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], ApiEvent::InstanceCreated { .. }));
        assert!(matches!(&events[1], ApiEvent::InstanceDeleted { .. }));
    }

    // -----------------------------------------------------------------------
    // Slice 4: Dispatch-Level Notifier Coverage
    // -----------------------------------------------------------------------
    // These tests exercise handle_session's actual notifier call sites by
    // starting a real API server with a RecordingNotifier and sending NDJSON
    // requests over TCP.

    /// Start an API server on a background thread with a given notifier.
    fn start_test_server_with(
        label: &str,
        notifier: Option<Arc<dyn ApiNotifier>>,
    ) -> (u16, std::path::PathBuf, Arc<AtomicBool>) {
        let home = tmp_home(label);
        let run_dir = crate::daemon::run_dir(&home);
        std::fs::create_dir_all(&run_dir).unwrap();
        crate::auth_cookie::issue(&run_dir).unwrap();

        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs: ConfigRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let externals: crate::agent::ExternalRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let h = home.clone();
        let r = Arc::clone(&registry);
        let s = Arc::clone(&shutdown);
        let c = Arc::clone(&configs);
        let e = Arc::clone(&externals);

        std::thread::Builder::new()
            .name(format!("test_api_{label}"))
            .spawn(move || {
                serve(
                    &h,
                    r,
                    s,
                    c,
                    e,
                    notifier,
                    crate::api::RestartCapability::Unsupported,
                    None,
                );
            })
            .unwrap();

        let mut port = 0u16;
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if let Ok(contents) = std::fs::read_to_string(run_dir.join("api.port")) {
                if let Ok(p) = contents.trim().parse::<u16>() {
                    port = p;
                    break;
                }
            }
        }
        assert!(port > 0, "API server did not publish port");
        (port, home, shutdown)
    }

    /// Start an API server with a RecordingNotifier.
    fn start_test_server(
        label: &str,
    ) -> (
        u16,
        std::path::PathBuf,
        Arc<RecordingNotifier>,
        Arc<AtomicBool>,
    ) {
        let rec = Arc::new(RecordingNotifier::new());
        let n: Arc<dyn ApiNotifier> = Arc::clone(&rec) as Arc<dyn ApiNotifier>;
        let (port, home, shutdown) = start_test_server_with(label, Some(n));
        (port, home, rec, shutdown)
    }

    /// Send an NDJSON request to the API server and read one response using a
    /// caller-supplied authenticated principal token.
    fn api_request_with_auth(
        port: u16,
        request: &Value,
        auth: &crate::auth_cookie::Cookie,
    ) -> Value {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let stream =
            std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = std::io::BufReader::new(stream);

        crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, auth).unwrap();

        writeln!(writer, "{}", request).unwrap();
        writer.flush().unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap_or(json!({"error": "parse failed"}))
    }

    /// Send an NDJSON request on the operator-capability surface.
    fn api_request(port: u16, home: &std::path::Path, request: &Value) -> Value {
        let run_dir = crate::daemon::run_dir(home);
        let operator_token = crate::auth_cookie::read_operator_token(&run_dir).unwrap();
        api_request_with_auth(port, request, &operator_token)
    }

    fn stop_server(shutdown: &Arc<AtomicBool>, home: &std::path::Path) {
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        // Connect to unblock the accept() loop
        let run_dir = crate::daemon::run_dir(home);
        if let Ok(contents) = std::fs::read_to_string(run_dir.join("api.port")) {
            if let Ok(port) = contents.trim().parse::<u16>() {
                let _ = std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    std::time::Duration::from_millis(100),
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    #[serial_test::serial]
    fn authenticated_agent_usage_limit_takeover_is_denied_at_api_ingress() {
        let (port, home, _notifier, shutdown) = start_test_server("usage-limit-api-gate");
        let run_dir = crate::daemon::run_dir(&home);
        let agent_cookie = crate::auth_cookie::read_cookie(&run_dir).unwrap();

        for mode in ["active", "away", "sleep"] {
            let mode_resp = api_request(
                port,
                &home,
                &json!({"method": "mode", "params": {"mode": mode}}),
            );
            assert_eq!(mode_resp["ok"], true, "mode setup failed: {mode_resp}");
            for instance in ["", "forged-operator"] {
                let response = api_request_with_auth(
                    port,
                    &json!({
                        "method": "mcp_tool",
                        "params": {
                            "tool": "usage_limit_takeover",
                            "instance": instance,
                            "arguments": {
                                "instance": "worker-a",
                                "episode_id": "forged"
                            }
                        }
                    }),
                    &agent_cookie,
                );
                assert_eq!(
                    response["ok"], false,
                    "agent request unexpectedly allowed: {response}"
                );
                assert_eq!(
                    response["denied_by"], "capability",
                    "denial must occur at authenticated API ingress: {response}"
                );
            }
        }

        let active_resp = api_request(
            port,
            &home,
            &json!({"method": "mode", "params": {"mode": "active"}}),
        );
        assert_eq!(active_resp["ok"], true, "mode reset failed: {active_resp}");
        let operator_response = api_request(
            port,
            &home,
            &json!({
                "method": "mcp_tool",
                "params": {
                    "tool": "usage_limit_takeover",
                    "instance": "",
                    "arguments": {
                        "instance": "worker-a",
                        "episode_id": "forged"
                    }
                }
            }),
        );
        assert_eq!(
            operator_response["ok"], true,
            "operator path was gated: {operator_response}"
        );
        assert_eq!(
            operator_response["result"]["error_code"], "binding_unreadable",
            "operator request must reach the usage-limit handler: {operator_response}"
        );

        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_delete_emits_instance_deleted() {
        let (port, home, notifier, shutdown) = start_test_server("dispatch-del");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "delete", "params": {"name": "agent-x"}}),
        );
        assert_eq!(resp["ok"], true);
        let events = notifier.take();
        assert_eq!(events.len(), 1, "expected 1 event, got {events:?}");
        let ApiEvent::InstanceDeleted { name } = &events[0] else {
            panic!("expected InstanceDeleted, got {:?}", events[0])
        };
        assert_eq!(name, "agent-x");
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_create_team_emits_team_created() {
        let (port, home, notifier, shutdown) = start_test_server("dispatch-team");
        // CREATE_TEAM with existing members (no spawns) now emits TeamCreated
        // with the full member roster.
        let resp = api_request(
            port,
            &home,
            &json!({
                "method": "create_team",
                "params": {"name": "test-team", "members": ["a", "b"]}
            }),
        );
        assert_eq!(resp["ok"], true);
        let events = notifier.take();
        assert_eq!(
            events.len(),
            1,
            "expected TeamCreated for existing members, got {events:?}"
        );
        let ApiEvent::TeamCreated { name, members } = &events[0] else {
            panic!("expected TeamCreated, got {:?}", events[0])
        };
        assert_eq!(name, "test-team");
        assert_eq!(members, &["a", "b"]);
        stop_server(&shutdown, &home);
    }

    /// Positive-pin companion to `dispatch_create_team_emits_team_created`:
    /// uses `true` (resolved via PATH — `/usr/bin/true` on macOS,
    /// `/bin/true` or `/usr/bin/true` on Linux) as a harmless real
    /// backend so `spawn_one` actually succeeds, exercising the
    /// spawn + emit path of `handle_create_team` end-to-end.
    /// `TeamCreated.members` now carries the full roster (all_members),
    /// not just spawned names.
    ///
    /// Context (see `LESSONS-04-21.md` open items): headless daemon mode
    /// passes `notifier = None`, so the emission block in
    /// `handlers/team.rs:153-161` is unreachable by the standard E2E
    /// smoke. Prior coverage used a three-piece equivalence bracket —
    /// (a) negative pin via the sibling test above, (b) byte-identical
    /// refactor verdict from at-dev-3 on the emission block, (c) runtime
    /// abort-point evidence from smoke logs. This positive pin replaces
    /// (a)+(c) with a direct in-process assertion that a successful
    /// spawn results in the expected `ApiEvent::TeamCreated` payload.
    ///
    /// `#[cfg(unix)]` — `true(1)` is a universal harmless real backend
    /// on Unix; Windows lacks a directly equivalent short-lived builtin
    /// and the LESSONS open item is scoped to a proof-of-concept. A
    /// Windows-specific positive pin can be added as a follow-up
    /// without changing this test.
    #[cfg(unix)]
    #[test]
    fn dispatch_create_team_emits_team_created_positive() {
        let (port, home, notifier, shutdown) = start_test_server("dispatch-team-pos");
        let resp = api_request(
            port,
            &home,
            &json!({
                "method": "create_team",
                "params": {
                    "name": "positive_pin",
                    "backend": "true",
                    "count": 1
                }
            }),
        );
        assert_eq!(resp["ok"], true, "create_team failed: {resp:?}");
        assert_eq!(
            resp["spawned"].as_array().map(|a| a.len()),
            Some(1),
            "expected 1 spawned agent, got {resp:?}"
        );
        // The notifier emission in `handle_create_team` happens synchronously
        // before the response is written to the wire (see handlers/team.rs
        // L153-161), so by the time `api_request` returns, the event is
        // already in `RecordingNotifier`'s buffer — no polling needed.
        let events = notifier.take();
        assert_eq!(events.len(), 1, "expected 1 event, got {events:?}");
        let ApiEvent::TeamCreated { name, members } = &events[0] else {
            panic!("expected TeamCreated, got {:?}", events[0])
        };
        assert_eq!(name, "positive_pin");
        assert_eq!(members, &["positive_pin-1"]);
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_update_team_emits_members_changed() {
        let (port, home, notifier, shutdown) = start_test_server("dispatch-update-team");
        // First create a team via the teams store
        // Sprint 54 fleet-yaml unification: teams live in fleet.yaml.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "teams:\n  t1:\n    members: [m1]\n    created_at: \"2026-01-01T00:00:00Z\"\n",
        )
        .unwrap();

        let resp = api_request(
            port,
            &home,
            &json!({
                "method": "update_team",
                "params": {"name": "t1", "add": ["m2"]}
            }),
        );
        assert_eq!(resp["ok"], true);
        let events = notifier.take();
        assert_eq!(events.len(), 1, "expected 1 event, got {events:?}");
        let ApiEvent::TeamMembersChanged {
            name,
            added,
            removed,
        } = &events[0]
        else {
            panic!("expected TeamMembersChanged, got {:?}", events[0])
        };
        assert_eq!(name, "t1");
        assert_eq!(added, &["m2"]);
        assert!(removed.is_empty());
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_delete_with_none_notifier_no_panic() {
        let (port, home, shutdown) = start_test_server_with("dispatch-none", None);
        let resp = api_request(
            port,
            &home,
            &json!({"method": "delete", "params": {"name": "ghost"}}),
        );
        assert_eq!(resp["ok"], true);
        stop_server(&shutdown, &home);
    }

    // -----------------------------------------------------------------------
    // Slice A characterization: LIST + STATUS response shapes
    // -----------------------------------------------------------------------

    #[test]
    fn dispatch_list_returns_agents_array_and_protocol_version() {
        let (port, home, _notifier, shutdown) = start_test_server("list-shape");
        let resp = api_request(port, &home, &json!({"method": "list"}));
        assert_eq!(resp["ok"], true);
        let result = &resp["result"];
        assert!(
            result["protocol_version"].is_number(),
            "expected protocol_version number, got: {result}"
        );
        let agents = result["agents"].as_array().expect("agents array");
        assert_eq!(agents.len(), 0, "empty registry should yield 0 agents");
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_status_returns_agents_and_timestamp() {
        let (port, home, _notifier, shutdown) = start_test_server("status-shape");
        let resp = api_request(port, &home, &json!({"method": "status"}));
        assert_eq!(resp["ok"], true);
        let result = &resp["result"];
        let agents = result["agents"].as_array().expect("agents array");
        assert_eq!(agents.len(), 0, "no snapshot should yield 0 agents");
        assert_eq!(result["timestamp"], serde_json::Value::Null);
        stop_server(&shutdown, &home);
    }

    // -----------------------------------------------------------------------
    // Slice D characterization: SEND + REGISTER/DEREGISTER_EXTERNAL
    // -----------------------------------------------------------------------

    #[test]
    fn dispatch_send_delivers_to_inbox() {
        let (port, home, _notifier, shutdown) = start_test_server("send-char");
        // Target must exist in fleet.yaml for validation to pass.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  receiver:\n    backend: claude\n",
        )
        .ok();
        let resp = api_request(
            port,
            &home,
            &json!({
                "method": "send",
                "params": {"from": "sender", "target": "receiver", "text": "hello"}
            }),
        );
        assert_eq!(resp["ok"], true);
        // Verify message was enqueued to receiver's inbox
        let inbox_file = crate::inbox::inbox_path_resolved(&home, "receiver");
        assert!(
            inbox_file.exists(),
            "expected inbox file at {}",
            inbox_file.display()
        );
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_send_rejects_self_send() {
        let (port, home, _notifier, shutdown) = start_test_server("send-self");
        let resp = api_request(
            port,
            &home,
            &json!({
                "method": "send",
                "params": {"from": "agent-x", "target": "agent-x", "text": "hi"}
            }),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .is_some_and(|e| e.contains("cannot send to self")),);
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_register_and_deregister_external() {
        let (port, home, _notifier, shutdown) = start_test_server("ext-reg");
        // Register
        let resp = api_request(
            port,
            &home,
            &json!({
                "method": "register_external",
                "params": {"name": "ext-agent", "backend": "custom", "pid": 12345}
            }),
        );
        assert_eq!(resp["ok"], true);

        // Verify it shows up in LIST
        let list_resp = api_request(port, &home, &json!({"method": "list"}));
        let agents = list_resp["result"]["agents"].as_array().expect("agents");
        assert!(
            agents
                .iter()
                .any(|a| a["name"] == "ext-agent" && a["kind"] == "external"),
            "expected ext-agent in list, got: {agents:?}"
        );

        // Deregister
        let resp = api_request(
            port,
            &home,
            &json!({"method": "deregister_external", "params": {"name": "ext-agent"}}),
        );
        assert_eq!(resp["ok"], true);

        // Verify it's gone from LIST
        let list_resp = api_request(port, &home, &json!({"method": "list"}));
        let agents = list_resp["result"]["agents"].as_array().expect("agents");
        assert!(
            !agents.iter().any(|a| a["name"] == "ext-agent"),
            "ext-agent should be gone after deregister"
        );

        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_deregister_nonexistent_returns_error() {
        let (port, home, _notifier, shutdown) = start_test_server("ext-dereg-miss");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "deregister_external", "params": {"name": "ghost"}}),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .is_some_and(|e| e.contains("not found")));
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_register_duplicate_returns_error() {
        let (port, home, _notifier, shutdown) = start_test_server("ext-dup");
        // Register first time
        let resp = api_request(
            port,
            &home,
            &json!({
                "method": "register_external",
                "params": {"name": "dup-agent", "backend": "custom", "pid": 1}
            }),
        );
        assert_eq!(resp["ok"], true);

        // Register same name again
        let resp = api_request(
            port,
            &home,
            &json!({
                "method": "register_external",
                "params": {"name": "dup-agent", "backend": "custom", "pid": 2}
            }),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .is_some_and(|e| e.contains("already exists")));

        stop_server(&shutdown, &home);
    }

    // -----------------------------------------------------------------------
    // Slice B characterization: INJECT + KILL + DELETE + SPAWN error branches
    // -----------------------------------------------------------------------
    // Convention: every writeln+continue → return conversion must have its
    // early-error branch pinned here.

    // -- INJECT --

    #[test]
    fn dispatch_inject_validate_name_fail() {
        let (port, home, _n, shutdown) = start_test_server("inject-badname");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "inject", "params": {"name": "../escape", "data": "x"}}),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"].as_str().is_some());
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_inject_agent_not_found() {
        let (port, home, _n, shutdown) = start_test_server("inject-notfound");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "inject", "params": {"name": "ghost", "data": "x"}}),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .is_some_and(|e| e.contains("not found")));
        stop_server(&shutdown, &home);
    }

    // -- KILL --

    #[test]
    fn dispatch_kill_validate_name_fail() {
        let (port, home, _n, shutdown) = start_test_server("kill-badname");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "kill", "params": {"name": "../escape"}}),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"].as_str().is_some());
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_kill_agent_not_found() {
        let (port, home, _n, shutdown) = start_test_server("kill-notfound");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "kill", "params": {"name": "ghost"}}),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .is_some_and(|e| e.contains("not found")));
        stop_server(&shutdown, &home);
    }

    // -- DELETE --

    #[test]
    fn dispatch_delete_validate_name_fail() {
        let (port, home, _n, shutdown) = start_test_server("delete-badname");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "delete", "params": {"name": "../escape"}}),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"].as_str().is_some());
        stop_server(&shutdown, &home);
    }

    // DELETE happy path already covered by dispatch_delete_emits_instance_deleted (Slice 4)

    // -- SPAWN --

    #[test]
    fn dispatch_spawn_missing_name() {
        let (port, home, _n, shutdown) = start_test_server("spawn-noname");
        let resp = api_request(port, &home, &json!({"method": "spawn", "params": {}}));
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .is_some_and(|e| e.contains("missing")));
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_spawn_validate_name_fail() {
        let (port, home, _n, shutdown) = start_test_server("spawn-badname");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "spawn", "params": {"name": "../escape"}}),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"].as_str().is_some());
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_spawn_backend_not_found() {
        let (port, home, _n, shutdown) = start_test_server("spawn-badbinary");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "spawn", "params": {"name": "test-agent", "backend": "nonexistent-binary-xyz"}}),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"].as_str().is_some());
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_spawn_working_directory_rejected() {
        let (port, home, _n, shutdown) = start_test_server("spawn-badwd");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "spawn", "params": {"name": "test-wd", "working_directory": "/tmp/../etc/foo"}}),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"].as_str().is_some());
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_delete_external_success() {
        let (port, home, _n, shutdown) = start_test_server("del-ext");
        // Register an external agent
        let _ = api_request(
            port,
            &home,
            &json!({"method": "register_external", "params": {"name": "ext-1", "backend": "x", "pid": 1}}),
        );
        // Delete it — exercises the external early-return success path
        let resp = api_request(
            port,
            &home,
            &json!({"method": "delete", "params": {"name": "ext-1"}}),
        );
        assert_eq!(resp["ok"], true);
        stop_server(&shutdown, &home);
    }

    // SPAWN happy path + dedup are disclosed known gaps — require real agent spawn

    // -----------------------------------------------------------------------
    // Slice C1 characterization: UPDATE_TEAM
    // -----------------------------------------------------------------------

    #[test]
    fn dispatch_update_team_missing_name() {
        let (port, home, _n, shutdown) = start_test_server("ut-noname");
        let resp = api_request(port, &home, &json!({"method": "update_team", "params": {}}));
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .is_some_and(|e| e.contains("missing")));
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_update_team_remove_member() {
        let (port, home, notifier, shutdown) = start_test_server("ut-remove");
        // Pre-create team with members
        // Sprint 54 fleet-yaml unification: teams live in fleet.yaml.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "teams:\n  t1:\n    members: [m1, m2]\n    created_at: \"2026-01-01T00:00:00Z\"\n",
        )
        .unwrap();

        let resp = api_request(
            port,
            &home,
            &json!({"method": "update_team", "params": {"name": "t1", "remove": ["m2"]}}),
        );
        assert_eq!(resp["ok"], true);
        let events = notifier.take();
        assert_eq!(events.len(), 1);
        let ApiEvent::TeamMembersChanged {
            name,
            added,
            removed,
        } = &events[0]
        else {
            panic!("expected TeamMembersChanged, got {:?}", events[0])
        };
        assert_eq!(name, "t1");
        assert!(added.is_empty());
        assert_eq!(removed, &["m2"]);
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_update_team_noop_no_event() {
        let (port, home, notifier, shutdown) = start_test_server("ut-noop");
        // Pre-create team
        // Sprint 54 fleet-yaml unification: teams live in fleet.yaml.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "teams:\n  t1:\n    members: [m1]\n    created_at: \"2026-01-01T00:00:00Z\"\n",
        )
        .unwrap();

        // Re-add existing member → noop diff → no event
        let resp = api_request(
            port,
            &home,
            &json!({"method": "update_team", "params": {"name": "t1", "add": ["m1"]}}),
        );
        assert_eq!(resp["ok"], true);
        let events = notifier.take();
        assert_eq!(events.len(), 0, "noop diff should not emit event");
        stop_server(&shutdown, &home);
    }

    // -----------------------------------------------------------------------
    // Slice C2 characterization: CREATE_TEAM
    // -----------------------------------------------------------------------

    #[test]
    fn dispatch_create_team_missing_name() {
        let (port, home, _n, shutdown) = start_test_server("ct-noname");
        let resp = api_request(port, &home, &json!({"method": "create_team", "params": {}}));
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .is_some_and(|e| e.contains("missing")));
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_create_team_all_spawns_failed() {
        let (port, home, _n, shutdown) = start_test_server("ct-allfail");
        let resp = api_request(
            port,
            &home,
            &json!({
                "method": "create_team",
                "params": {
                    "name": "fail-team",
                    "backend": "nonexistent-binary-xyz",
                    "count": 2
                }
            }),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"].as_str().is_some_and(|e| e.contains("failed")));
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_create_team_zero_count_succeeds() {
        let (port, home, notifier, shutdown) = start_test_server("ct-zero");
        let resp = api_request(
            port,
            &home,
            &json!({
                "method": "create_team",
                "params": {"name": "empty-team"}
            }),
        );
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["spawned"], json!([]));
        assert!(resp.get("failed").is_none(), "no failed field expected");
        // Empty team (no members) → no TeamCreated event
        let events = notifier.take();
        assert_eq!(events.len(), 0, "empty team should not emit TeamCreated");
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_create_team_with_existing_members_only() {
        let (port, home, notifier, shutdown) = start_test_server("ct-members");
        let resp = api_request(
            port,
            &home,
            &json!({
                "method": "create_team",
                "params": {"name": "ref-team", "members": ["a", "b"]}
            }),
        );
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["spawned"], json!([]));
        // Existing members now emit TeamCreated with full roster.
        let events = notifier.take();
        assert_eq!(
            events.len(),
            1,
            "expected TeamCreated for existing members, got {events:?}"
        );
        let ApiEvent::TeamCreated { name, members } = &events[0] else {
            panic!("expected TeamCreated, got {:?}", events[0])
        };
        assert_eq!(name, "ref-team");
        assert_eq!(members, &["a", "b"]);
        stop_server(&shutdown, &home);
    }

    // -----------------------------------------------------------------------
    // MOVE_PANE dispatch coverage
    // -----------------------------------------------------------------------

    #[test]
    fn dispatch_move_pane_missing_agent() {
        let (port, home, _n, shutdown) = start_test_server("mp-noagent");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "move_pane", "params": {"target_tab": "t"}}),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .is_some_and(|e| e.contains("missing agent")));
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_move_pane_missing_target_tab() {
        let (port, home, _n, shutdown) = start_test_server("mp-notab");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "move_pane", "params": {"agent": "a"}}),
        );
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .is_some_and(|e| e.contains("missing target_tab")));
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_move_pane_emits_pane_moved_default_horizontal() {
        let (port, home, notifier, shutdown) = start_test_server("mp-emit-h");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "move_pane", "params": {"agent": "a1", "target_tab": "team-x"}}),
        );
        assert_eq!(resp["ok"], true);
        let events = notifier.take();
        assert_eq!(events.len(), 1, "expected 1 event, got {events:?}");
        let ApiEvent::PaneMoved {
            agent,
            target_tab,
            split_dir,
        } = &events[0]
        else {
            panic!("expected PaneMoved, got {:?}", events[0])
        };
        assert_eq!(agent, "a1");
        assert_eq!(target_tab, "team-x");
        assert_eq!(*split_dir, PaneMoveSplitDir::Horizontal);
        stop_server(&shutdown, &home);
    }

    #[test]
    fn dispatch_move_pane_parses_vertical_split() {
        let (port, home, notifier, shutdown) = start_test_server("mp-emit-v");
        let resp = api_request(
            port,
            &home,
            &json!({"method": "move_pane", "params": {
                "agent": "a2",
                "target_tab": "team-y",
                "split_dir": "vertical"
            }}),
        );
        assert_eq!(resp["ok"], true);
        let events = notifier.take();
        assert_eq!(events.len(), 1);
        let ApiEvent::PaneMoved { split_dir, .. } = &events[0] else {
            panic!("expected PaneMoved");
        };
        assert_eq!(*split_dir, PaneMoveSplitDir::Vertical);
        stop_server(&shutdown, &home);
    }

    // -----------------------------------------------------------------------
    // spawn_one preset resolution — regression pin for per-backend submit_key
    // -----------------------------------------------------------------------

    #[test]
    fn spawn_one_resolves_preset_submit_key() {
        // Verify that Backend::from_command returns the correct preset
        // submit_key for each known backend. spawn_one uses this to avoid
        // hardcoding "\r" where a backend needs a different submit sequence.
        use crate::backend::Backend;
        let cases = [
            ("claude", "\r"),
            ("kiro-cli", "\r"),
            ("codex", "\r"),
            ("agy", "\r"),
        ];
        for (cmd, expected) in cases {
            let key = Backend::from_command(cmd)
                .map(|b| b.preset().submit_key)
                .unwrap_or("\r");
            assert_eq!(
                key, expected,
                "backend '{cmd}' should have submit_key {expected:?}, got {key:?}"
            );
        }
        // Unknown backend falls back to "\r"
        let unknown = Backend::from_command("unknown-backend")
            .map(|b| b.preset().submit_key)
            .unwrap_or("\r");
        assert_eq!(unknown, "\r");
    }

    // ── #bughunt-r1 #4: accept-loop error disposition ──────────────────────

    #[test]
    fn accept_error_disposition_logs_first_then_rate_limits() {
        // First error in a streak always logs; subsequent ones are suppressed
        // until the next `ACCEPT_ERROR_LOG_EVERY` multiple — so a persistent
        // failure produces a bounded log rate, not one line per spin.
        assert_eq!(accept_error_disposition(1), (true, false), "1st error logs");
        assert_eq!(
            accept_error_disposition(2),
            (false, false),
            "2nd suppressed"
        );
        assert_eq!(
            accept_error_disposition(ACCEPT_ERROR_LOG_EVERY),
            (true, false),
            "every Nth logs"
        );
        assert_eq!(
            accept_error_disposition(ACCEPT_ERROR_LOG_EVERY + 1),
            (false, false),
            "N+1 suppressed"
        );
    }

    #[test]
    fn accept_error_disposition_breaks_after_sustained_failure() {
        // Below the cap → keep going; at/over the cap → give up the loop.
        assert!(
            !accept_error_disposition(MAX_CONSECUTIVE_ACCEPT_ERRORS - 1).1,
            "just under cap keeps accepting"
        );
        assert!(
            accept_error_disposition(MAX_CONSECUTIVE_ACCEPT_ERRORS).1,
            "at cap breaks the accept loop"
        );
    }

    // ── #bughunt-r1 #2: api::call port+cookie single run-dir resolution ────

    /// Source-scan invariant (the runtime TOCTOU needs a daemon-restart race
    /// that's impractical to drive in a unit test): `api::call` MUST resolve the
    /// active run dir exactly ONCE and feed that same dir to BOTH the port
    /// connect (`connect_run_dir_api`) and the cookie read (`read_cookie`). It
    /// must NOT use `connect_api` (which does its OWN internal resolution) — that
    /// was the bug: a second `find_active_run_dir` for the cookie could land on a
    /// different run dir mid-restart.
    #[test]
    fn call_resolves_run_dir_once_for_port_and_cookie_bughunt_r1() {
        let src = include_str!("mod.rs");
        // Scope to the `pub fn call(` body. Stop at the next top-level item
        // (`\nfn ` — the following `api_call_read_timeout`) so the scan can NOT
        // reach the `#[cfg(test)]` module, whose own source contains the literal
        // `connect_api(` in this test's assert message (#1593 self-match trap).
        let start = src
            .find("pub fn call(home: &Path")
            .expect("call fn present");
        let after = &src[start..];
        let end = after[1..]
            .find("\nfn ")
            .map(|i| i + 1)
            .unwrap_or(after.len());
        let body = &after[..end];

        assert!(
            !body.contains("connect_api("),
            "#bughunt-r1 #2: api::call must NOT use connect_api (it re-resolves the \
             run dir internally → port/cookie TOCTOU); use connect_run_dir_api on a \
             single resolution"
        );
        assert!(
            body.contains("connect_run_dir_api("),
            "api::call must connect via connect_run_dir_api on the resolved run dir"
        );
        assert_eq!(
            body.matches("find_active_run_dir(").count(),
            1,
            "api::call must resolve the active run dir exactly ONCE (port + cookie \
             from the same dir)"
        );
    }

    /// bug-audit Rank1 regression: a `handle_session` PANIC must NOT leak the
    /// `active_conns` reservation. Pre-fix the manual `fetch_sub` sat AFTER
    /// `handle_session`, so an unwind skipped it and leaked the slot (leaks
    /// accumulate to `API_MAX_CONNS` → control-plane lockup). The `ConnSlot`
    /// `Drop` releases the slot on the unwind path, returning the count to
    /// baseline.
    #[test]
    fn conn_slot_releases_reservation_on_panic_unwind() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = Arc::new(AtomicUsize::new(0));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (_slot, prev) = ConnSlot::reserve(&counter);
            assert_eq!(prev, 0, "first reservation sees prev=0");
            assert_eq!(counter.load(Ordering::SeqCst), 1, "slot reserved");
            panic!("simulated handle_session panic");
        }));
        assert!(result.is_err(), "panic must propagate out of catch_unwind");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "ConnSlot::drop must release the slot on panic unwind (no leak)"
        );
    }

    /// bug-audit Rank1 regression (sibling counter): a `handle_session` panic
    /// must also decrement the in-flight session count via `SessionCount::drop`
    /// instead of leaking it.
    #[test]
    fn session_count_decrements_on_panic_unwind() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static TEST_SESSIONS: AtomicUsize = AtomicUsize::new(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _session = SessionCount::enter(&TEST_SESSIONS);
            assert_eq!(TEST_SESSIONS.load(Ordering::Relaxed), 1, "session entered");
            panic!("simulated handle_session panic");
        }));
        assert!(result.is_err(), "panic must propagate");
        assert_eq!(
            TEST_SESSIONS.load(Ordering::Relaxed),
            0,
            "SessionCount::drop must decrement on panic unwind"
        );
    }
}
