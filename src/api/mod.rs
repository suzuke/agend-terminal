//! Daemon JSON control API over TCP loopback.
//!
//! Protocol: NDJSON (one JSON request per line, one JSON response per line).
//! Port is published to `{run_dir}/api.port`; see `ipc.rs` for the port
//! registry and loopback-binding rules.

use crate::agent::{AgentRegistry, ExternalRegistry};
use crate::tasks::operator_settlement as settlement;
use anyhow::Context;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
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

// ---------------------------------------------------------------------------
// ApiNotifier — decouples api.rs from the TUI layer
// ---------------------------------------------------------------------------

/// Domain events emitted by the API server when agents or teams change.
/// These are independent of any UI representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(dead_code)] // retained as the API notifier compatibility payload
pub enum ApiEvent {
    InstanceCreated {
        name: String,
        instance_ref: Option<crate::types::InstanceRef>,
        /// Correlates a daemon-owned restart with its request. Ordinary
        /// spawns leave this unset; name alone is never a lifecycle proof.
        restart_id: Option<String>,
        old_instance_ref: Option<crate::types::InstanceRef>,
        layout: LayoutHint,
        spawner: Option<String>,
        target_pane: Option<String>,
    },
    InstanceDeleted {
        name: String,
        instance_ref: Option<crate::types::InstanceRef>,
        /// Correlates the delete leg of a daemon-owned restart. Ordinary
        /// deletions leave this unset for backwards-compatible retirement.
        #[serde(default)]
        restart_id: Option<String>,
    },
    /// A daemon-owned restart could not create its successor. The TUI uses
    /// the exact predecessor identity to retain the pane as disconnected
    /// immediately instead of waiting for correlation expiry.
    InstanceRestartFailed {
        name: String,
        restart_id: String,
        old_instance_ref: Option<crate::types::InstanceRef>,
        error: String,
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
    /// A persisted team/fleet configuration change that is not represented by
    /// a membership diff. The TUI responds by requesting a Live roster.
    ConfigChanged {
        name: String,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneMoveSplitDir {
    #[default]
    Horizontal,
    Vertical,
}

/// Layout hint for newly created instances. Parsed at the API boundary so
/// invalid values are caught early rather than silently defaulting downstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
/// how (or whether) to react. The daemon event hub fans them out to
/// authenticated stream subscribers; other owners may drop them.
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
    pub const SUBSCRIBE_EVENTS: &str = "subscribe_events";
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
    /// No shipped composition root constructs this yet; keep the typed capability
    /// reserved until that owner-restart slice is wired.
    #[allow(dead_code)]
    App,
    /// Any other API-server owner (e.g. `verify`) — default-deny. A DISTINCT
    /// value from `App` even though R1 routes both to a fail-closed response.
    Unsupported,
}

/// Start API socket server (blocks calling thread).
///
/// `notifier`: when running inside the TUI app, `Some(notifier)` to notify the
/// event loop about instance/team creation and deletion. The daemon composition
/// root uses [`serve_with_ready_events`] to provide the notifier and stream.
/// This legacy entry point passes `None`, so it does not expose events.
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
    serve_inner(
        home,
        registry,
        shutdown,
        configs,
        externals,
        notifier,
        host,
        app_restart,
        None,
        None,
        None,
    );
}

/// Daemon-owned API server entry that reports when the listener is fully ready.
///
/// Unlike [`serve`], this lets `run_core` wait until the port has been published
/// and the authentication material has been loaded before it starts fleet
/// agents. The bounded wait lives at the daemon composition root; this function
/// reports either readiness or the exact startup failure once.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub(crate) fn serve_with_ready(
    home: &Path,
    registry: AgentRegistry,
    shutdown: Arc<AtomicBool>,
    configs: ConfigRegistry,
    externals: ExternalRegistry,
    notifier: Option<Arc<dyn ApiNotifier>>,
    host: RestartCapability,
    app_restart: Option<crate::api::app_restart::AppRestart>,
    ready_tx: std::sync::mpsc::SyncSender<Result<(), String>>,
    shutdown_wake: Option<crossbeam_channel::Sender<()>>,
) {
    serve_inner(
        home,
        registry,
        shutdown,
        configs,
        externals,
        notifier,
        host,
        app_restart,
        Some(ready_tx),
        shutdown_wake,
        None,
    );
}

/// Daemon-owned API entry with a lifecycle event stream. The dedicated stream
/// uses a separate authenticated connection so ordinary request/response calls
/// never receive unsolicited frames.
#[allow(clippy::too_many_arguments)]
pub(crate) fn serve_with_ready_events(
    home: &Path,
    registry: AgentRegistry,
    shutdown: Arc<AtomicBool>,
    configs: ConfigRegistry,
    externals: ExternalRegistry,
    event_hub: Arc<crate::daemon::event_hub::EventHub>,
    host: RestartCapability,
    app_restart: Option<crate::api::app_restart::AppRestart>,
    ready_tx: std::sync::mpsc::SyncSender<Result<(), String>>,
    shutdown_wake: Option<crossbeam_channel::Sender<()>>,
) {
    let notifier: Arc<dyn ApiNotifier> = event_hub.clone();
    serve_inner(
        home,
        registry,
        shutdown,
        configs,
        externals,
        Some(notifier),
        host,
        app_restart,
        Some(ready_tx),
        shutdown_wake,
        Some(event_hub),
    );
}

fn report_startup_failure(
    ready_tx: &mut Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
    error: &str,
) {
    if let Some(tx) = ready_tx.take() {
        let _ = tx.send(Err(error.to_string()));
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_inner(
    home: &Path,
    registry: AgentRegistry,
    shutdown: Arc<AtomicBool>,
    configs: ConfigRegistry,
    externals: ExternalRegistry,
    notifier: Option<Arc<dyn ApiNotifier>>,
    host: RestartCapability,
    app_restart: Option<crate::api::app_restart::AppRestart>,
    mut ready_tx: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
    shutdown_wake: Option<crossbeam_channel::Sender<()>>,
    event_hub: Option<Arc<crate::daemon::event_hub::EventHub>>,
) {
    // #945 Phase 0: time the bind+port-publish step directly (not the
    // spawn of api::serve thread — that's sub-ms). Operators care about
    // "when did api.port appear" for cold-start latency tracking.
    let _api_port_bind_start = std::time::Instant::now();
    let listener: TcpListener = match crate::ipc::bind_loopback() {
        Ok(l) => l,
        Err(e) => {
            report_startup_failure(&mut ready_tx, &format!("failed to bind API socket: {e}"));
            tracing::warn!(error = %e, "failed to bind API socket");
            return;
        }
    };
    let port = crate::ipc::local_port(&listener);
    let run_dir = crate::daemon::run_dir(home);
    if let Err(e) = crate::ipc::write_port(&run_dir, crate::ipc::API_NAME, port) {
        report_startup_failure(&mut ready_tx, &format!("failed to publish API port: {e}"));
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
            report_startup_failure(&mut ready_tx, &format!("api.cookie missing: {e}"));
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
            report_startup_failure(&mut ready_tx, &format!("api.operator token missing: {e}"));
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
    if let Some(tx) = ready_tx.take() {
        let _ = tx.send(Ok(()));
    }

    // #1189: write `.ready` in app (TUI) mode after confirmed bind success.
    // Daemon mode writes `.ready` later (after spawn loop) with richer semantics.
    if notifier.is_some() && event_hub.is_none() {
        if let Err(e) = crate::ready::write(&run_dir) {
            tracing::warn!(error = %e, "failed to write .ready marker");
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
                    let _ = crate::resource_limits::record_fd_exhaustion(home, "api_accept", &e);
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
        let session_shutdown_wake = shutdown_wake.clone();
        let session_event_hub = event_hub.clone();
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
                    ntf.as_ref(),
                    session_operator_token,
                    session_cookie,
                    session_host,
                    session_app_restart,
                    session_shutdown_wake,
                    session_event_hub,
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
    notifier: Option<&Arc<dyn ApiNotifier>>,
    operator_token: crate::auth_cookie::Cookie,
    cookie: crate::auth_cookie::Cookie,
    host: RestartCapability,
    app_restart: Option<crate::api::app_restart::AppRestart>,
    shutdown_wake: Option<crossbeam_channel::Sender<()>>,
    event_hub: Option<Arc<crate::daemon::event_hub::EventHub>>,
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
    // Telemetry only — see the bridge contract in docs/MCP-TOOLS.md.
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
        // Event streaming is a direct API method, so apply the authenticated
        // principal's capability before entering the long-lived stream.
        if method == method::SUBSCRIBE_EVENTS
            && operator_gate::capability_allows_request(principal, method, params)
        {
            let Some(hub) = event_hub.as_ref() else {
                let _ = writeln!(
                    writer,
                    "{}",
                    json!({"ok": false, "error": "event stream unavailable"})
                );
                let _ = writer.flush();
                continue;
            };
            let receiver = hub.subscribe();
            let hello = json!({
                "ok": true,
                "event_stream": {"source": hub.source()},
            });
            if writeln!(writer, "{hello}").is_err() || writer.flush().is_err() {
                break;
            }
            while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
                match receiver.recv_timeout(std::time::Duration::from_millis(250)) {
                    Ok(event) => {
                        let line = match serde_json::to_string(&event) {
                            Ok(line) => line,
                            Err(_) => break,
                        };
                        if writeln!(writer, "{line}").is_err() || writer.flush().is_err() {
                            break;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
            break;
        }
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
            shutdown: Some(Arc::clone(shutdown)),
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
            let token_hash = crate::daemon::utils::sha256_hex(&operator_token);
            request_dedup::global().dispatch(
                request_id,
                request_dedup::operation_fingerprint(method, params),
                request_dedup::method_wait_timeout(method, params),
                || match method {
                    "task_settlement_preview" => settlement::preview(home, params, &token_hash),
                    "task_settlement_apply" => settlement::apply(home, params, &token_hash),
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
                    method::MCP_TOOL => {
                        handlers::mcp_proxy::handle_mcp_tool_with_operator_authority(
                            params,
                            &ctx,
                            principal == crate::auth_cookie::Principal::Operator,
                        )
                    }
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
                        if let Some(wake) = shutdown_wake.as_ref() {
                            let _ = wake.try_send(());
                        }
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
/// Performs the P1-10 cookie handshake using the mode-0600 credential in the
/// supplied run dir and rejects calls without successful authentication.
/// #1814: like [`call`] but targets a SPECIFIC run dir (cookie + api.port read
/// from `run_dir`), not the active daemon. Used by the self-respawn Phase-1
/// gate against the successor's control plane with a caller-selected deadline.
/// No self-IPC guard: this connects to a DIFFERENT process's socket.
pub fn call_at(
    run_dir: &Path,
    request: &Value,
    timeout: std::time::Duration,
) -> anyhow::Result<Value> {
    let stream = crate::ipc::connect_run_dir_api(run_dir)?;
    stream
        .set_read_timeout(Some(timeout))
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

#[cfg(test)]
pub(crate) static FORBID_LOOPBACK_3573: parking_lot::Mutex<Option<std::path::PathBuf>> =
    parking_lot::Mutex::new(None);

pub fn call(home: &Path, request: &Value) -> anyhow::Result<Value> {
    #[cfg(test)]
    assert_ne!(
        FORBID_LOOPBACK_3573.lock().as_deref(),
        Some(home),
        "unexpected loopback in runtime restart"
    );
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
#[path = "readiness_tests.rs"]
mod readiness_tests;

#[cfg(test)]
mod working_directory_smoke_tests;

#[cfg(test)]
mod operator_settlement_tests;

#[cfg(test)]
mod tests;
