//! Daemon JSON control API over TCP loopback.
//!
//! Protocol: NDJSON (one JSON request per line, one JSON response per line).
//! Port is published to `{run_dir}/api.port`; see `ipc.rs` for the port
//! registry and loopback-binding rules.

use crate::agent::{self, AgentRegistry, ExternalRegistry};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

mod handlers;

pub type ConfigRegistry = Arc<Mutex<HashMap<String, crate::daemon::AgentConfig>>>;

// ---------------------------------------------------------------------------
// ApiNotifier — decouples api.rs from the TUI layer
// ---------------------------------------------------------------------------

/// Domain events emitted by the API server when agents or teams change.
/// These are independent of any UI representation.
#[derive(Debug, Clone)]
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
}

impl LayoutHint {
    pub fn parse(s: &str) -> Self {
        match s {
            "split-right" => Self::SplitRight,
            "split-below" => Self::SplitBelow,
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

/// Validate a caller-supplied `working_directory` against the AGEND_HOME and
/// (optionally) `AGEND_ALLOWED_WORK_ROOTS` — a platform-native path list
/// (`:`-separated on Unix, `;`-separated on Windows, same rules as `PATH`).
///
/// Rules:
/// - Path must not contain `..` components (blocks relative escape regardless
///   of whether the target exists).
/// - After canonicalising the deepest existing ancestor, the resolved path
///   must start with one of the allowed roots. This catches symlink escape
///   inside an otherwise-legal prefix.
///
/// Returns the resolved `PathBuf` on success. The caller is responsible for
/// creating the directory.
pub fn validate_working_directory(
    path: &std::path::Path,
    home: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    use std::path::{Component, PathBuf};
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        anyhow::bail!("working_directory must not contain '..'");
    }
    // Walk up to the deepest existing ancestor for canonicalisation. A path
    // pointing into a not-yet-created subdirectory is legal as long as its
    // existing prefix is inside an allowed root.
    let mut existing = path.to_path_buf();
    while !existing.as_os_str().is_empty() && !existing.exists() {
        if !existing.pop() {
            break;
        }
    }
    let anchor = if existing.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        std::fs::canonicalize(&existing).unwrap_or(existing.clone())
    };
    let tail = path.strip_prefix(&existing).unwrap_or(path);
    let resolved = anchor.join(tail);

    let mut roots: Vec<PathBuf> = Vec::new();
    roots.push(std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()));
    if let Some(extra) = std::env::var_os("AGEND_ALLOWED_WORK_ROOTS") {
        // `split_paths` uses the OS-native separator — `:` on Unix, `;` on
        // Windows. Raw `split(':')` broke Windows because `C:\...` paths
        // already contain a colon after the drive letter.
        for pb in std::env::split_paths(&extra).filter(|p| !p.as_os_str().is_empty()) {
            roots.push(std::fs::canonicalize(&pb).unwrap_or(pb));
        }
    }
    for root in &roots {
        if resolved.starts_with(root) {
            return Ok(strip_verbatim_prefix(resolved));
        }
    }
    anyhow::bail!(
        "working_directory '{}' escapes allowed roots (set AGEND_ALLOWED_WORK_ROOTS to widen)",
        resolved.display()
    )
}

/// On Windows, `std::fs::canonicalize` returns `\\?\C:\...` (the Win32
/// extended-length path form). PTY spawn hands this straight to `cmd.exe` as
/// its cwd, and cmd bails with "UNC paths are not supported" before ever
/// running — codex surfaces this as "default directory is Windows".
/// Strip the verbatim prefix when it names a plain drive so the returned path
/// is what cmd expects. UNC shares (`\\?\UNC\server\share`) are left alone —
/// cmd can't cd into those regardless, and a caller that needs UNC semantics
/// should see the failure explicitly rather than get a subtly-rewritten path.
///
/// No-op on Unix.
fn strip_verbatim_prefix(path: std::path::PathBuf) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            // `\\?\C:\...` → `C:\...`, but leave `\\?\UNC\...` alone.
            if !rest.starts_with(r"UNC\") {
                return std::path::PathBuf::from(rest.to_string());
            }
        }
    }
    path
}

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
    pub const UPDATE_TEAM: &str = "update_team";
    pub const MOVE_PANE: &str = "move_pane";
    pub const SHUTDOWN: &str = "shutdown";
}

/// Start API socket server (blocks calling thread).
///
/// `notifier`: when running inside the TUI app, `Some(notifier)` to notify the
/// event loop about instance/team creation and deletion. Daemon mode passes
/// `None` and events are silently dropped.
pub fn serve(
    home: &Path,
    registry: AgentRegistry,
    shutdown: Arc<AtomicBool>,
    configs: ConfigRegistry,
    externals: ExternalRegistry,
    notifier: Option<Arc<dyn ApiNotifier>>,
) {
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
    tracing::info!(port, "API listening");

    for stream in listener.incoming().flatten() {
        let _ = stream.set_nodelay(true);
        // Slow-client hardening: set read/write deadlines so a stalled peer
        // cannot pin a session thread indefinitely. 30s is generous for a
        // JSON request line; control-plane calls are never slow on purpose.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(30)));
        let reg = Arc::clone(&registry);
        let home = home.to_path_buf();
        let shutdown = Arc::clone(&shutdown);
        let cfgs = Arc::clone(&configs);
        let ext = Arc::clone(&externals);
        let ntf = notifier.clone();
        // Cookie is `[u8; 32]` (Copy), each session gets its own copy so the
        // spawned closure satisfies `'static`.
        let session_cookie = cookie;
        std::thread::Builder::new()
            .name("api_handler".into())
            .spawn(move || {
                handle_session(
                    stream,
                    &reg,
                    &home,
                    &shutdown,
                    &cfgs,
                    &ext,
                    ntf.as_deref(),
                    session_cookie,
                )
            })
            .ok();
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
    cookie: crate::auth_cookie::Cookie,
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
    if let Err(e) = crate::auth_cookie::server_handshake_ndjson(&mut reader, &mut writer, &cookie) {
        tracing::warn!(error = %e, "API auth rejected");
        return;
    }

    loop {
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

        let ctx = handlers::HandlerCtx {
            registry,
            configs,
            externals,
            notifier,
            home,
        };

        let response = match method {
            method::LIST => handlers::query::handle_list(params, &ctx),
            method::INJECT => handlers::instance::handle_inject(params, &ctx),
            method::KILL => handlers::instance::handle_kill(params, &ctx),
            method::DELETE => handlers::instance::handle_delete(params, &ctx),
            method::SPAWN => handlers::instance::handle_spawn(params, &ctx),
            method::SEND => handlers::messaging::handle_send(params, &ctx),
            method::STATUS => handlers::query::handle_status(params, &ctx),
            method::REGISTER_EXTERNAL => handlers::external::handle_register_external(params, &ctx),
            method::DEREGISTER_EXTERNAL => {
                handlers::external::handle_deregister_external(params, &ctx)
            }
            method::CREATE_TEAM => handlers::team::handle_create_team(params, &ctx),
            method::UPDATE_TEAM => handlers::team::handle_update_team(params, &ctx),
            method::MOVE_PANE => handlers::instance::handle_move_pane(params, &ctx),
            method::SHUTDOWN => {
                tracing::info!("API shutdown requested");
                shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
                json!({"ok": true})
            }
            _ => json!({"ok": false, "error": format!("unknown method: {method}")}),
        };

        let _ = writeln!(writer, "{}", response);
        let _ = writer.flush();
    }
}

/// Spawn a single agent, register it, and start its TUI socket thread.
/// Shared by the SPAWN and CREATE_TEAM API handlers.
#[allow(clippy::too_many_arguments)]
fn spawn_one(
    home: &Path,
    registry: &AgentRegistry,
    name: &str,
    backend: &str,
    args: &[String],
    spawn_mode: crate::backend::SpawnMode,
    work_dir: &Path,
    size: (u16, u16),
) -> anyhow::Result<()> {
    std::fs::create_dir_all(work_dir).ok();
    agent::spawn_agent(
        &agent::SpawnConfig {
            name,
            backend_command: backend,
            args,
            spawn_mode,
            cols: size.0,
            rows: size.1,
            env: None,
            working_dir: Some(work_dir),
            submit_key: "\r",
            home: Some(home),
            crash_tx: None,
            shutdown: None,
        },
        registry,
    )?;
    let rdir = crate::daemon::run_dir(home);
    let reg = Arc::clone(registry);
    let n = name.to_string();
    std::thread::Builder::new()
        .name(format!("{n}_tui"))
        .spawn(move || crate::daemon::serve_agent_tui(&n, &rdir, &reg))
        .ok();
    Ok(())
}

/// Send a request to the daemon API and read one NDJSON response.
///
/// Performs the P1-10 cookie handshake first: reads `api.cookie` from the
/// active daemon's run dir, sends `{"auth":"<hex>"}`, and rejects the call
/// if the server does not reply `{"ok":true}`. The cookie file has mode
/// 0600 so only the daemon's user can read it — this is the peer-UID
/// substitute for TCP loopback (see `auth_cookie.rs`).
pub fn call(home: &Path, request: &Value) -> anyhow::Result<Value> {
    let stream = crate::ipc::connect_api(home)?;
    let run = crate::daemon::find_active_run_dir(home)
        .ok_or_else(|| anyhow::anyhow!("no active daemon (run dir not found)"))?;
    let cookie = crate::auth_cookie::read_cookie(&run)?;

    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, &cookie)?;

    writeln!(writer, "{}", request)?;
    writer.flush()?;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    let resp: Value = serde_json::from_str(line.trim())?;
    Ok(resp)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Serializes tests that mutate `AGEND_ALLOWED_WORK_ROOTS` — env mutation
    /// from parallel tests races otherwise.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        crate::sync::lock_poisoned(LOCK.get_or_init(|| Mutex::new(())), "api_env_guard")
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
    fn validate_work_dir_allows_under_home() {
        let home = tmp_home("validate_home");
        let ok = home.join("workspace").join("agent");
        let resolved =
            validate_working_directory(&ok, &home).expect("path under home must validate");
        // Returned path has any `\\?\` verbatim prefix stripped (see
        // `strip_verbatim_prefix`), so compare against the stripped form
        // of canonical home.
        let home_simplified = strip_verbatim_prefix(std::fs::canonicalize(&home).unwrap());
        assert!(
            resolved.starts_with(&home_simplified),
            "resolved {} should start with {}",
            resolved.display(),
            home_simplified.display()
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Regression guard for the "cmd.exe: UNC paths are not supported" bug.
    /// `std::fs::canonicalize` on Windows returns `\\?\C:\...` and handing
    /// that to a PTY makes cmd.exe refuse to launch. `validate_working_directory`
    /// must strip the verbatim prefix before returning so the resolved path
    /// round-trips through a Command spawn.
    #[test]
    fn validate_work_dir_strips_verbatim_prefix_from_return() {
        let home = tmp_home("validate_verbatim");
        let ok = home.join("project");
        let resolved = validate_working_directory(&ok, &home).expect("validate");
        #[cfg(windows)]
        {
            let s = resolved.to_string_lossy();
            assert!(
                !s.starts_with(r"\\?\"),
                "verbatim prefix must be stripped, got: {s}"
            );
        }
        // On Unix strip_verbatim_prefix is a no-op — just sanity that the
        // function still returns something that starts with home.
        #[cfg(unix)]
        {
            let home_canon = std::fs::canonicalize(&home).unwrap();
            assert!(resolved.starts_with(&home_canon));
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(windows)]
    #[test]
    fn strip_verbatim_prefix_handles_drive_and_leaves_unc() {
        use std::path::PathBuf;
        // `\\?\C:\...` → `C:\...`
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"\\?\C:\Users\alice")),
            PathBuf::from(r"C:\Users\alice")
        );
        // `\\?\UNC\server\share` must be preserved — simplifying to
        // `\\server\share` doesn't help cmd, and silently rewriting a share
        // path would be surprising.
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"\\?\UNC\server\share\dir")),
            PathBuf::from(r"\\?\UNC\server\share\dir")
        );
        // Regular drive path unaffected.
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"C:\plain\path")),
            PathBuf::from(r"C:\plain\path")
        );
    }

    #[test]
    fn validate_work_dir_rejects_outside_home() {
        let _g = env_guard();
        let home = tmp_home("validate_outside");
        // Pick a path that definitely exists and is not under home.
        let outside = std::path::PathBuf::from("/tmp");
        // Ensure AGEND_ALLOWED_WORK_ROOTS isn't accidentally opening this up.
        let prev = std::env::var("AGEND_ALLOWED_WORK_ROOTS").ok();
        std::env::remove_var("AGEND_ALLOWED_WORK_ROOTS");
        let err = validate_working_directory(&outside, &home).unwrap_err();
        if let Some(v) = prev {
            std::env::set_var("AGEND_ALLOWED_WORK_ROOTS", v);
        }
        assert!(
            format!("{err}").contains("escapes"),
            "expected escape rejection, got: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn validate_work_dir_honors_allowed_roots_env() {
        let _g = env_guard();
        let home = tmp_home("validate_env_root");
        let root = std::env::temp_dir().join(format!(
            "agend-extra-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("mkdir extra root");
        let inside = root.join("agent");
        let prev = std::env::var("AGEND_ALLOWED_WORK_ROOTS").ok();
        std::env::set_var("AGEND_ALLOWED_WORK_ROOTS", root.display().to_string());
        let result = validate_working_directory(&inside, &home);
        match prev {
            Some(v) => std::env::set_var("AGEND_ALLOWED_WORK_ROOTS", v),
            None => std::env::remove_var("AGEND_ALLOWED_WORK_ROOTS"),
        }
        result.expect("path under AGEND_ALLOWED_WORK_ROOTS must validate");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn validate_work_dir_rejects_symlink_escape() {
        // Stage 4 P1-8 regression guard: a symlink inside an allowed root
        // pointing OUT of all allowed roots must be rejected after canonicalisation.
        let _g = env_guard();
        let home = tmp_home("validate_symlink_escape");
        // Create a symlink at `{home}/escape` → /tmp (outside any allowed root).
        let target = std::path::PathBuf::from("/tmp");
        let link = home.join("escape");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");

        let prev = std::env::var("AGEND_ALLOWED_WORK_ROOTS").ok();
        std::env::remove_var("AGEND_ALLOWED_WORK_ROOTS");
        // Request a path *under* the symlink. After canonicalisation it should
        // resolve outside `home` and be rejected.
        let requested = link.join("agent");
        let result = validate_working_directory(&requested, &home);
        if let Some(v) = prev {
            std::env::set_var("AGEND_ALLOWED_WORK_ROOTS", v);
        }
        match result {
            Ok(resolved) => panic!(
                "expected symlink escape rejection, but validated as {}",
                resolved.display()
            ),
            Err(e) => assert!(
                format!("{e}").contains("escapes"),
                "expected escape rejection, got: {e}"
            ),
        }
        std::fs::remove_dir_all(&home).ok();
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

    // -----------------------------------------------------------------------
    // ApiNotifier seam tests
    // -----------------------------------------------------------------------

    /// Test-only notifier that records every event for later assertion.
    struct RecordingNotifier {
        events: std::sync::Mutex<Vec<ApiEvent>>,
    }

    impl RecordingNotifier {
        fn new() -> Self {
            Self {
                events: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn take(&self) -> Vec<ApiEvent> {
            std::mem::take(&mut *self.events.lock().expect("recording lock"))
        }
    }

    impl ApiNotifier for RecordingNotifier {
        fn notify(&self, event: ApiEvent) {
            self.events.lock().expect("recording lock").push(event);
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
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let configs: ConfigRegistry =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let externals: crate::agent::ExternalRegistry =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let h = home.clone();
        let r = Arc::clone(&registry);
        let s = Arc::clone(&shutdown);
        let c = Arc::clone(&configs);
        let e = Arc::clone(&externals);

        std::thread::Builder::new()
            .name(format!("test_api_{label}"))
            .spawn(move || {
                serve(&h, r, s, c, e, notifier);
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

    /// Send an NDJSON request to the API server and read one response.
    fn api_request(port: u16, home: &std::path::Path, request: &Value) -> Value {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let stream =
            std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut reader = std::io::BufReader::new(stream);

        // Auth handshake
        let run_dir = crate::daemon::run_dir(home);
        let cookie = crate::auth_cookie::read_cookie(&run_dir).unwrap();
        crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, &cookie).unwrap();

        writeln!(writer, "{}", request).unwrap();
        writer.flush().unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap_or(json!({"error": "parse failed"}))
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
        let store_path = home.join("teams.json");
        std::fs::write(
            &store_path,
            r#"{"schema_version":1,"teams":[{"name":"t1","members":["m1"],"created_at":"2026-01-01T00:00:00Z"}]}"#,
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
        let inbox_file = home.join("inbox").join("receiver.jsonl");
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
            &json!({"method": "spawn", "params": {"name": "test-wd", "working_directory": "/etc/foo"}}),
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
        let store_path = home.join("teams.json");
        std::fs::write(
            &store_path,
            r#"{"schema_version":1,"teams":[{"name":"t1","members":["m1","m2"],"created_at":"2026-01-01T00:00:00Z"}]}"#,
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
        let store_path = home.join("teams.json");
        std::fs::write(
            &store_path,
            r#"{"schema_version":1,"teams":[{"name":"t1","members":["m1"],"created_at":"2026-01-01T00:00:00Z"}]}"#,
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
}
