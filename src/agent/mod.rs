//! Agent state and PTY management.
//!
//! Sync design: std::thread for PTY I/O, crossbeam broadcast for output distribution.
//! Single Mutex on AgentCore ensures atomic subscribe+dump.

use crate::backend::Backend;
use crate::health::HealthTracker;
use crate::state::StateTracker;
use crate::vterm::VTerm;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

mod dismiss;
pub use dismiss::try_dismiss_dialog;

pub type PtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Core state for one agent — protected by a single Mutex for atomic operations.
pub struct AgentCore {
    pub(crate) vterm: VTerm,
    pub(crate) subscribers: Vec<crossbeam_channel::Sender<Vec<u8>>>,
    pub(crate) state: StateTracker,
    pub(crate) health: HealthTracker,
}

/// Handle to interact with an agent.
#[allow(dead_code)]
pub struct AgentHandle {
    pub(crate) id: crate::types::InstanceId,
    pub(crate) name: crate::types::AgentName,
    pub(crate) backend_command: String,
    pub(crate) pty_writer: PtyWriter,
    pub(crate) pty_master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub(crate) core: Arc<Mutex<AgentCore>>,
    pub(crate) child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    pub(crate) submit_key: String,
    pub(crate) inject_prefix: String,
    pub(crate) typed_inject: bool,
    pub(crate) spawned_at: std::time::Instant,
    pub(crate) spawned_at_epoch_ms: u64,
    /// Set by DELETE handler to prevent reaper from spawning shell fallback.
    pub(crate) deleted: Arc<std::sync::atomic::AtomicBool>,
}

pub type AgentRegistry = Arc<Mutex<HashMap<String, AgentHandle>>>;

/// Handle for an externally connected agent (not PTY-managed by daemon).
pub struct ExternalAgentHandle {
    pub(crate) backend_command: String,
    pub(crate) pid: u32,
}

pub type ExternalRegistry = Arc<Mutex<HashMap<String, ExternalAgentHandle>>>;

/// Lock the external registry, recovering from poison.
pub fn lock_external(
    reg: &ExternalRegistry,
) -> parking_lot::MutexGuard<'_, HashMap<String, ExternalAgentHandle>> {
    reg.lock()
}

/// Environment variable names that fleet.yaml-supplied `env:` maps are NOT
/// allowed to override when spawning an agent. These either (a) carry
/// credentials that only the host user should control, (b) govern dynamic
/// linking and would let a hostile fleet.yaml load attacker-supplied code
/// into the spawned process, or (c) are agend's own runtime plumbing.
///
/// Matching is case-insensitive for cross-platform safety: Windows env is
/// case-insensitive, so `anthropic_api_key` and `ANTHROPIC_API_KEY` map to the
/// same variable there, and a pure case-sensitive deny-list would miss it.
const SENSITIVE_ENV_KEYS: &[&str] = &[
    // API credentials for backends we drive
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    // Cloud credentials commonly present in dev environments
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    // Git forge tokens
    "GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "NPM_TOKEN",
    // Dynamic-linker injection vectors (Linux / macOS)
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    // agend's own runtime wiring — overriding these lets a template redirect
    // the spawned agent to a different home / break MCP config discovery
    "AGEND_HOME",
    "AGEND_INSTANCE_NAME",
    "AGEND_ALLOWED_WORK_ROOTS",
    "AGEND_MCP_TOOLS_ALLOW",
    "AGEND_MCP_TOOLS_DENY",
];

/// Returns true if the env-var name is on the spawn-time deny-list.
pub fn is_sensitive_env_key(key: &str) -> bool {
    SENSITIVE_ENV_KEYS
        .iter()
        .any(|denied| denied.eq_ignore_ascii_case(key))
}

/// Validate and sanitize an instance name. Only allows [a-zA-Z0-9_-].
pub fn validate_name(name: &str) -> Result<&str, String> {
    if name.is_empty() {
        return Err("instance name cannot be empty".into());
    }
    if name.len() > 64 {
        return Err("instance name too long (max 64 chars)".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "instance name '{}' contains invalid characters (only a-z, 0-9, -, _ allowed)",
            name
        ));
    }
    Ok(name)
}

/// [`validate_name`] with a JSON error response for MCP handlers.
/// Use in functions that return `serde_json::Value`:
/// `validate_name_or_err!(name)` expands to an early return on failure.
#[macro_export]
macro_rules! validate_name_or_err {
    ($name:expr) => {
        if let Err(e) = $crate::agent::validate_name($name) {
            return serde_json::json!({"error": e});
        }
    };
}

/// Error from [`resolve_instance`].
#[derive(Debug)]
pub enum ResolveError {
    NotFound(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(t) => write!(f, "instance '{t}' not found"),
        }
    }
}

/// Resolve a name-or-id string to `(InstanceId, display_name)` via fleet.yaml.
///
/// Resolution order per PLAN §3.3:
/// 1. Exact full-UUID match
/// 2. Exact 8-char short-id prefix match
/// 3. Exact instance name match
///
/// fleet.yaml `instances` is a `HashMap<String, _>` so name uniqueness is
/// structurally guaranteed — no Ambiguous error path needed.
pub fn resolve_instance(
    home: &std::path::Path,
    name_or_id: &str,
) -> Result<(crate::types::InstanceId, String), ResolveError> {
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .unwrap_or_default();

    // 1. Full UUID match
    if let Some(id) = crate::types::InstanceId::parse(name_or_id) {
        for (name, inst) in &fleet.instances {
            if inst.id.as_deref().and_then(crate::types::InstanceId::parse) == Some(id.clone()) {
                return Ok((id, name.clone()));
            }
        }
        return Err(ResolveError::NotFound(name_or_id.to_string()));
    }

    // 2. Short-id prefix match (8 hex chars)
    if name_or_id.len() == 8 && name_or_id.chars().all(|c| c.is_ascii_hexdigit()) {
        for (name, inst) in &fleet.instances {
            if let Some(ref id_str) = inst.id {
                if let Some(id) = crate::types::InstanceId::parse(id_str) {
                    if id.short() == name_or_id {
                        return Ok((id, name.clone()));
                    }
                }
            }
        }
        // Fall through to name match
    }

    // 3. Exact name match — HashMap guarantees at most one entry per name.
    if let Some(inst) = fleet.instances.get(name_or_id) {
        let id = inst
            .id
            .as_deref()
            .and_then(crate::types::InstanceId::parse)
            .unwrap_or_default();
        return Ok((id, name_or_id.to_string()));
    }

    Err(ResolveError::NotFound(name_or_id.to_string()))
}

/// Lock the agent registry, recovering from poison.
pub fn lock_registry(
    reg: &AgentRegistry,
) -> parking_lot::MutexGuard<'_, std::collections::HashMap<String, AgentHandle>> {
    crate::sync_audit::assert_lock_tier(1, "registry");
    reg.lock()
}

// ── #945 Phase 1: pending-registry slot for deferred attach ────────────
//
// `bootstrap::telegram_init` runs in a background thread (~6s of HTTP
// calls). When it completes, it needs to call `Channel::attach_registry`
// against the agent registry that the caller (run_core / app::run)
// creates separately. Pre-#945 the caller did this synchronously via
// `if let Some(tg) = prepared.telegram { tg.attach_registry(...) }`
// at `daemon/mod.rs:443-447` (and analogous `app/mod.rs:213-222`); but
// post-backgrounding `prepared.telegram` is None at boot.
//
// The pending-registry slot bridges the gap: caller publishes its
// registry via `set_pending_registry`; the background telegram_init
// thread polls `get_pending_registry` after `register_active_channel`
// and calls `attach_registry` when the registry is available.
//
// Single-writer per process (run_core OR app::run — they're mutually
// exclusive entry points). `OnceLock` enforces this: first caller
// wins; subsequent `set_pending_registry` calls silently no-op.
static PENDING_REGISTRY: std::sync::OnceLock<AgentRegistry> = std::sync::OnceLock::new();

/// Publish the agent registry for deferred attach by the background
/// `telegram_init` thread. Idempotent — subsequent calls no-op.
/// Caller is `run_core` (daemon mode) or `app::run` (TUI mode).
pub fn set_pending_registry(reg: AgentRegistry) {
    let _ = PENDING_REGISTRY.set(reg);
}

/// Read the registry published by `set_pending_registry`. Returns
/// `None` if no caller has published yet. Background
/// `telegram_init` polls this after `register_active_channel`.
pub fn get_pending_registry() -> Option<AgentRegistry> {
    PENDING_REGISTRY.get().cloned()
}

/// #941: registry-lock wrapper that records the holder for the periodic
/// thread-dump observability handler. Use this in per_tick handler call
/// sites where wedge-detection matters; bare [`lock_registry`] is
/// retained for the ~30 other in-tree sites (wrapper-only blind spot —
/// see PR body for caveat).
///
/// The `site` label is `&'static str` so the dump output can group
/// holders by call-site without allocation overhead. Convention: snake
/// case matching the handler name (`"hang_detection"`, `"watchdog"`,
/// etc.) so operators grepping the dump output can match against the
/// per-tick handler vec.
///
/// Zero overhead when `AGEND_DAEMON_THREAD_DUMP_SECS` is unset:
/// `set_registry_holder` / `clear_registry_holder` early-return after
/// one cached atomic load.
pub fn lock_registry_tracked<'a>(reg: &'a AgentRegistry, site: &'static str) -> RegistryGuard<'a> {
    crate::sync_audit::assert_lock_tier(1, "registry");
    let inner = reg.lock();
    crate::sync_audit::set_registry_holder(site);
    RegistryGuard { inner }
}

/// RAII guard returned by [`lock_registry_tracked`]. Deref's to the
/// underlying `HashMap<String, AgentHandle>`. On drop, clears
/// `REGISTRY_HOLDER` then the inner `MutexGuard` releases the lock
/// (field drop order). The brief slot-cleared-before-lock-released
/// window is harmless — the next acquirer's own `set_registry_holder`
/// fires immediately after their acquire.
pub struct RegistryGuard<'a> {
    inner: parking_lot::MutexGuard<'a, std::collections::HashMap<String, AgentHandle>>,
}

impl<'a> std::ops::Deref for RegistryGuard<'a> {
    type Target = std::collections::HashMap<String, AgentHandle>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a> std::ops::DerefMut for RegistryGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'a> Drop for RegistryGuard<'a> {
    fn drop(&mut self) {
        crate::sync_audit::clear_registry_holder();
    }
}

/// ANSI escape sequence stripper for dialog detection.
/// Public ANSI strip for capture command.
pub fn strip_ansi_pub(s: &str) -> String {
    strip_ansi(s)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        chars.next();
                        if ch.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // G1: OSC sequence — terminated by BEL (\x07) or ST (\x1b\\)
                    chars.next();
                    while let Some(&ch) = chars.peek() {
                        if ch == '\x07' {
                            chars.next();
                            break;
                        }
                        if ch == '\x1b' {
                            chars.next();
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        chars.next();
                    }
                }
                Some('(' | ')') => {
                    chars.next();
                    chars.next();
                }
                _ => {
                    chars.next();
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Exit event sent from the PTY reaper to the daemon main loop.
#[derive(Debug, Clone)]
pub enum AgentExitEvent {
    /// Agent crashed or exited unexpectedly — daemon should respawn.
    Crash(String),
    /// Agent exited cleanly (exit code 0, e.g. `/exit` or `/quit`) — no respawn.
    CleanExit(String),
    /// `#685` sub-task 7a: emitted by the recovery dispatcher when Stage 2
    /// auto-restart fires after Stage 1 ESC fails to clear `Hung` within
    /// the timeout window. Semantically distinct from `Crash` so the
    /// respawn worker can skip the crash-counter increment (Stage 2
    /// is recovery-initiated, not a process crash). Phase 1 (sub-task
    /// 7a / Stage 1) ships the variant definition only — emission and
    /// handler logic land in sub-task 7b (Stage 2 PR). Until then this
    /// variant is constructed only by tests pinning the channel shape.
    #[allow(dead_code)]
    Stage2Restart(String),
}

/// Channel for exit events from reaper to daemon.
pub type CrashChannel = crossbeam_channel::Sender<AgentExitEvent>;

/// Configuration for spawning an agent.
///
/// `args` are **user-only**: the caller passes whatever they'd add on top of
/// the backend's baseline (e.g. `--model foo`), and `spawn_agent` prepends the
/// preset args dictated by `spawn_mode`. Callers should not compose
/// `--trust-all-tools`, `--dangerously-skip-permissions`, etc. themselves —
/// otherwise those flags get double-applied.
pub struct SpawnConfig<'a> {
    pub name: &'a str,
    pub backend_command: &'a str,
    pub args: &'a [String],
    pub spawn_mode: crate::backend::SpawnMode,
    pub cols: u16,
    pub rows: u16,
    pub env: Option<&'a HashMap<String, String>>,
    pub working_dir: Option<&'a std::path::Path>,
    pub submit_key: &'a str,
    pub home: Option<&'a std::path::Path>,
    pub crash_tx: Option<CrashChannel>,
    pub shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Build a `CommandBuilder` with resolved args, env, and working directory.
///
/// Extracted from `spawn_agent` so the command-construction logic (arg
/// enrichment, env filtering, PATH prepend, cwd validation) is isolated
/// from the PTY plumbing that follows.
fn build_command(config: &SpawnConfig) -> anyhow::Result<(CommandBuilder, Option<Backend>)> {
    let SpawnConfig {
        name,
        backend_command,
        args,
        spawn_mode,
        working_dir,
        env,
        home,
        ..
    } = config;

    let detected_backend = Backend::from_command(backend_command);

    // argv = preset (per spawn_mode) + caller args + backend spawn_flags.
    // Centralized here so callers don't double-apply preset args.
    let enriched_args: Vec<String> = {
        let preset = detected_backend
            .as_ref()
            .map(|b| b.preset_spawn_args(*spawn_mode))
            .unwrap_or_default();
        let flags = detected_backend
            .as_ref()
            .zip(*working_dir)
            .map(|(b, wd)| b.spawn_flags(wd))
            .unwrap_or_default();
        preset
            .into_iter()
            .chain(args.iter().cloned())
            .chain(flags)
            .collect()
    };

    // Resolve bare command names to absolute paths via `which` before handing
    // them to `CommandBuilder`. On Windows, npm global installs drop both a
    // Unix-style shell-script (no extension) and a `.cmd` wrapper in the same
    // directory; `CreateProcessW`'s PATHEXT search walks the exact name first
    // and picks the extensionless Unix script, which blows up with
    // ERROR_BAD_EXE_FORMAT (193). Pre-resolving gives us the `.cmd` path
    // unambiguously. On Unix this is a no-op — `execvp` already does the
    // equivalent PATH walk — but keeping the same code path on both platforms
    // avoids a `#[cfg(windows)]` split here.
    let resolved_command =
        which::which(backend_command).unwrap_or_else(|_| std::path::PathBuf::from(backend_command));
    let mut cmd = CommandBuilder::new(&resolved_command);
    cmd.args(&enriched_args);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("FORCE_COLOR", "1");
    cmd.env("AGEND_INSTANCE_NAME", name);

    // Phase A Piece-3: GIT_EDITOR + friends = `true` (Unix no-op
    // binary that exits 0 without producing output). Prevents git
    // editor-needing operations from dropping the agent's PTY into
    // a Vim/editor lockup when the agent runs `git rebase --continue`
    // / `git commit` (without `-m`) / `git rebase -i` etc. The
    // empirical experiment (5-backend × 4-scenario) hit this on
    // opencode + DeepSeek when `rebase --continue` opened the
    // commit-message editor.
    //
    // Cover the full git editor-resolution chain (per `man git-var`:
    // GIT_EDITOR → core.editor → VISUAL → EDITOR → vi). Setting only
    // GIT_EDITOR is insufficient if a child script does
    // `git -c core.editor=` (unsets), so VISUAL/EDITOR are defensive
    // fallbacks. GIT_SEQUENCE_EDITOR covers `rebase -i`'s todo file
    // editor specifically.
    //
    // Operator override path preserved: these are set BEFORE the
    // fleet.yaml user-env loop below, so an operator setting
    // `instances.<name>.env.GIT_EDITOR: vim` (or any other value)
    // will override the daemon default in the same loop.
    cmd.env("GIT_EDITOR", "true");
    cmd.env("GIT_SEQUENCE_EDITOR", "true");
    cmd.env("EDITOR", "true");
    cmd.env("VISUAL", "true");

    if std::env::var("LANG").is_err() {
        cmd.env("LANG", "en_US.UTF-8");
    }

    // User env from fleet.yaml. Drop entries on the sensitive-env deny-list
    // so a hostile template cannot override ANTHROPIC_API_KEY, LD_PRELOAD,
    // AGEND_HOME, etc. with attacker-controlled values inherited by the
    // spawned agent process.
    if let Some(env_map) = *env {
        for (k, v) in env_map {
            if is_sensitive_env_key(k) {
                tracing::warn!(
                    instance = %name,
                    key = %k,
                    "dropping fleet.yaml env override for sensitive key"
                );
                continue;
            }
            cmd.env(k, v);
        }
    }

    // Add agend-terminal binary + $AGEND_HOME/bin (shim) to PATH.
    // Shim dir goes first so agend-git shadows /usr/bin/git.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            let mut paths: Vec<PathBuf> = Vec::new();
            // Phase 2: prepend $AGEND_HOME/bin/ for git shim shadowing.
            if let Some(h) = home {
                let shim_dir = h.join("bin");
                paths.push(shim_dir);
            }
            paths.push(bin_dir.to_path_buf());
            if let Some(existing) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                cmd.env("PATH", joined);
            }
        }
    }

    // Phase 1 git-shim: inject AGEND_REAL_GIT so the shim can exec the
    // real git binary without recursion (R12 mitigation).
    // Excludes $AGEND_HOME/bin/ from PATH to avoid resolving to the shim itself.
    if std::env::var("AGEND_REAL_GIT").is_err() {
        let agend_bin = home
            .map(|h| h.join("bin").display().to_string())
            .unwrap_or_default();
        let search_paths: Vec<PathBuf> = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .filter(|p| !p.is_empty() && *p != agend_bin)
            .map(PathBuf::from)
            .collect();
        let search = std::env::join_paths(&search_paths).unwrap_or_default();
        if let Ok(git_path) = which::which_in("git", Some(search), ".") {
            cmd.env("AGEND_REAL_GIT", git_path);
        }
    }

    if let Some(dir) = working_dir {
        // Defense-in-depth: the API spawn handler already calls
        // validate_working_directory at admission, but a symlink could have
        // been swapped in between admission and spawn. Revalidate here both
        // before and after create_dir_all so the final cwd we hand to the PTY
        // provably resolves inside AGEND_HOME / AGEND_ALLOWED_WORK_ROOTS.
        // If no home is available (ad-hoc test spawn), skip the recheck.
        if let Some(home_path) = *home {
            if let Err(e) = crate::api::validate_working_directory(dir, home_path) {
                anyhow::bail!("working_directory validation failed at spawn: {e}");
            }
            std::fs::create_dir_all(dir).ok();
            // Second pass: now that the leaf exists, canonicalisation walks
            // through any symlink and the starts_with check inside the
            // validator catches escape-via-symlink.
            let resolved = crate::api::validate_working_directory(dir, home_path)
                .map_err(|e| anyhow::anyhow!("working_directory escapes via symlink: {e}"))?;
            cmd.cwd(&resolved);
        } else {
            tracing::warn!(
                instance = %name,
                dir = %dir.display(),
                "spawn without AGEND_HOME — working_directory symlink recheck skipped"
            );
            std::fs::create_dir_all(dir).ok();
            cmd.cwd(dir);
        }
    }

    // #708: strip AGEND_GIT_BYPASS from child env — agents must use the
    // git shim (which checks the var), not inherit a blanket bypass.
    cmd.env_remove("AGEND_GIT_BYPASS");

    Ok((cmd, detected_backend))
}

pub fn spawn_agent(config: &SpawnConfig, registry: &AgentRegistry) -> anyhow::Result<()> {
    let SpawnConfig {
        name,
        backend_command,
        args: _,
        spawn_mode: _,
        cols,
        rows,
        env: _,
        working_dir,
        submit_key,
        home,
        crash_tx,
        shutdown,
    } = config;

    let (cmd, detected_backend) = build_command(config)?;

    // #995 Bug 3: emit a warning when spawning a backend whose MCP
    // discovery is incompatible with fleet's `<workdir>/.<vendor>/mcp_config.json`
    // writes. The `agend-mcp-bridge` will be configured on disk but the
    // backend will ignore it — the spawned instance has no `send`/`inbox`/
    // `task` MCP tools. Currently only Backend::Agy is affected (see
    // BackendPreset::fleet_mcp_supported docstring for the empirical
    // background + upstream tracking).
    //
    // Operator-visible via app.log. UI-banner injection would race the
    // backend's own ready output and is deliberately not attempted here.
    if let Some(backend) = detected_backend.as_ref() {
        if !backend.preset().fleet_mcp_supported {
            tracing::warn!(
                target: "fleet_mcp_unsupported",
                agent = %name,
                backend = backend.as_str(),
                "⚠️  [fleet-mcp-unsupported] this backend currently doesn't load \
                 the agend-mcp-bridge — fleet `send`/`inbox`/`task` tools will be \
                 unavailable in this instance. Awaiting upstream fix. Use this \
                 instance for manual / non-fleet work only."
            );
        }
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: *rows,
            cols: *cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow::anyhow!("Failed to open PTY: {e}"))?;

    // RAII guard arms the rollback for partial-failure between here and the
    // commit() at the end of this fn. Sprint 20 F1: previously a take_writer /
    // try_clone_reader / pty_read_loop spawn failure left an orphan PID (no
    // registry entry) or a phantom registry entry (no read thread).
    let mut rollback = crate::daemon::lifecycle::SpawnRollback::new(name, registry);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| anyhow::anyhow!("Failed to spawn '{backend_command}': {e}"))?;
    drop(pair.slave);
    let child_arc: Arc<Mutex<Box<dyn portable_pty::Child + Send>>> = Arc::new(Mutex::new(child));
    rollback.mark_child_spawned(Arc::clone(&child_arc));

    let pty_writer: PtyWriter = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("take_writer: {e}"))?,
    ));
    let mut pty_reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("clone_reader: {e}"))?;
    let pty_master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));

    let core = Arc::new(Mutex::new(AgentCore {
        vterm: VTerm::with_pty_writer(*cols, *rows, Arc::clone(&pty_writer)),
        subscribers: Vec::new(),
        state: StateTracker::new(detected_backend.as_ref()),
        health: HealthTracker::new(),
    }));

    // Register in registry
    {
        // Sprint 46 P2: resolve instance ID from fleet.yaml (backfilled by P1).
        let instance_id = config
            .home
            .and_then(|h| crate::fleet::FleetConfig::load(&h.join("fleet.yaml")).ok())
            .and_then(|c| {
                c.instances
                    .get(*name)
                    .and_then(|i| i.id.as_deref())
                    .and_then(crate::types::InstanceId::parse)
            })
            .unwrap_or_default();
        let mut reg = registry.lock();
        reg.insert(
            name.to_string(),
            AgentHandle {
                id: instance_id,
                name: name.to_string().into(),
                backend_command: backend_command.to_string(),
                pty_writer: Arc::clone(&pty_writer),
                pty_master: Arc::clone(&pty_master),
                core: Arc::clone(&core),
                child: Arc::clone(&child_arc),
                submit_key: submit_key.to_string(),
                inject_prefix: detected_backend
                    .as_ref()
                    .map(|b| b.preset().inject_prefix.to_string())
                    .unwrap_or_default(),
                typed_inject: detected_backend
                    .as_ref()
                    .map(|b| b.preset().typed_inject)
                    .unwrap_or(false),
                spawned_at: std::time::Instant::now(),
                spawned_at_epoch_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );
    }
    rollback.mark_registered();

    // PTY read thread — feeds VTerm + broadcasts + auto-dismiss trust dialog + session reaper
    let core2 = Arc::clone(&core);
    let pw = Arc::clone(&pty_writer);
    let reg_for_reaper = Arc::clone(registry);
    let home_for_reaper = home.map(|p| p.to_path_buf());
    let crash_tx_for_reaper = crash_tx.clone();
    let dismiss: Vec<(String, Vec<u8>)> = detected_backend
        .as_ref()
        .map(|b| {
            b.preset()
                .dismiss_patterns
                .iter()
                .map(|dp| (dp.label.to_string(), dp.sequence.to_vec()))
                .collect()
        })
        .unwrap_or_default();
    let shutdown_for_reaper = shutdown.clone();
    let deleted_for_reaper = {
        let reg = registry.lock();
        reg.get(*name)
            .map(|h| Arc::clone(&h.deleted))
            .unwrap_or_default()
    };
    let n = name.to_string();
    let ctx = PtyReadContext {
        name: n.clone(),
        core: core2,
        pty_writer: pw,
        registry: reg_for_reaper,
        home: home_for_reaper,
        crash_tx: crash_tx_for_reaper,
        dismiss_patterns: dismiss,
        shutdown: shutdown_for_reaper,
        deleted: deleted_for_reaper,
    };
    let capture = {
        let backend_str = detected_backend
            .as_ref()
            .map(|b| b.name())
            .unwrap_or(backend_command);
        crate::capture::make_capture_writer(home.as_deref(), name, backend_str)
    };
    // fire-and-forget: pty_read_loop terminates on PTY EOF, which fires when
    // the child process is killed during shutdown / delete. JoinHandle is
    // discarded because the loop's exit is signalled via the OS-side PTY
    // close, not via a stored handle.
    std::thread::Builder::new()
        .name(format!("{n}_pty_read"))
        .spawn(move || {
            pty_read_loop(&mut pty_reader, &ctx, capture);
        })?;

    // Backends whose CLI does not auto-load the instructions file (e.g. Kiro)
    // need the file contents injected as the first user message on Ready.
    if let Some(b) = detected_backend.as_ref() {
        let preset = b.preset();
        if preset.inject_instructions_on_ready {
            if let Some(dir) = working_dir {
                // Read the instructions body here — while we hold the spawn
                // context and before the `Ready` poll window starts — so an
                // external process mutating the file between write and
                // bootstrap cannot inject a different prompt. Skip the
                // bootstrap entirely if the file is missing/empty.
                let path = dir.join(preset.instructions_path);
                match std::fs::read_to_string(&path) {
                    Ok(content) if !content.trim().is_empty() => {
                        spawn_instructions_bootstrap(
                            Arc::clone(registry),
                            name.to_string(),
                            content,
                            std::time::Duration::from_secs(preset.ready_timeout_secs + 15),
                            shutdown.clone(),
                        );
                    }
                    Ok(_) => {
                        tracing::warn!(
                            agent = %name,
                            path = %path.display(),
                            "instructions file empty, skipping bootstrap inject"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            agent = %name,
                            path = %path.display(),
                            error = %e,
                            "instructions file unreadable, skipping bootstrap inject"
                        );
                    }
                }
            }
        }
    }

    // Sprint 52: register PTY subscriber with the router for mirror dispatch.
    // Done here (spawn site) so the router thread never needs L1/L2.
    {
        let reg = lock_registry(registry);
        if let Some(handle) = reg.get(name.to_string().as_str()) {
            let (tx, rx) = crossbeam_channel::bounded(1024);
            handle.core.lock().subscribers.push(tx);
            crate::daemon::router::register_agent(name, rx);
        }
    }

    // Disarm the rollback guard — all ordered mutations succeeded.
    rollback.commit();

    tracing::info!(agent = name, backend = backend_command, args = %config.args.join(" "), "spawned");
    Ok(())
}

/// Poll until the agent reaches Ready, then inject the pre-read instructions
/// content as a first user message. Used by backends (Kiro) whose CLI does
/// not auto-load the steering file.
///
/// The `content` is captured at spawn time (see call site) rather than
/// re-read after Ready: this closes the mutation window where an external
/// process could swap the instructions file between write and inject.
fn spawn_instructions_bootstrap(
    registry: AgentRegistry,
    name: String,
    content: String,
    timeout: std::time::Duration,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    let thread_name = format!("{name}_instr_boot");
    // fire-and-forget: instruction-bootstrap thread polls Ready then injects
    // the snapshotted instructions content. Observes shutdown flag inside the
    // poll loop (returns early on shutdown). JoinHandle dropped because the
    // thread is short-lived and one missed bootstrap on shutdown is cosmetic.
    let spawn_result = std::thread::Builder::new().name(thread_name).spawn(move || {
        let _census = crate::thread_census::register("instr_boot"); // M3: was "pty_reader"
        let deadline = std::time::Instant::now() + timeout;
        let poll_interval = std::time::Duration::from_millis(200);

        loop {
            if let Some(ref s) = shutdown {
                if s.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    agent = %name,
                    "instructions bootstrap timed out waiting for Ready"
                );
                return;
            }

            let ready = {
                let reg = &registry.lock();
                match reg.get(&name) {
                    Some(h) => {
                        let core = &h.core.lock();
                        core.state.get_state() == crate::state::AgentState::Ready
                    }
                    None => return, // agent gone
                }
            };
            if ready {
                break;
            }
            std::thread::sleep(poll_interval);
        }

        // Small settle delay so the prompt is fully painted before we type.
        // This is a UI-layer concern (avoiding a torn prompt paint) — the
        // content itself was already snapshotted at spawn, so the delay no
        // longer widens an external-mutation window.
        std::thread::sleep(std::time::Duration::from_millis(500));

        let reg = &registry.lock();
        if let Some(handle) = reg.get(&name) {
            if let Err(e) = inject_to_agent(handle, content.as_bytes()) {
                tracing::warn!(agent = %name, error = %e, "instructions bootstrap inject failed");
            } else {
                tracing::info!(
                    agent = %name,
                    bytes = content.len(),
                    "instructions bootstrap injected"
                );
            }
        }
    });
    if let Err(e) = spawn_result {
        tracing::warn!(error = %e, "failed to spawn instructions bootstrap thread");
    }
}

/// Context for PTY read loop reaper (reduces argument count).
struct PtyReadContext {
    name: String,
    core: Arc<Mutex<AgentCore>>,
    pty_writer: PtyWriter,
    registry: AgentRegistry,
    home: Option<std::path::PathBuf>,
    crash_tx: Option<CrashChannel>,
    dismiss_patterns: Vec<(String, Vec<u8>)>,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
    deleted: Arc<std::sync::atomic::AtomicBool>,
}

/// PTY read loop: feeds VTerm, broadcasts output, auto-dismisses dialogs, handles exit.
fn pty_read_loop(
    pty_reader: &mut dyn Read,
    ctx: &PtyReadContext,
    mut capture: Box<dyn crate::capture::CaptureWriter + Send>,
) {
    let PtyReadContext {
        name,
        core,
        pty_writer,
        registry,
        home,
        crash_tx,
        dismiss_patterns,
        shutdown,
        deleted,
    } = ctx;
    let mut buf = [0u8; 8192];
    let mut dismiss_cooldown_until: Option<std::time::Instant> = None;
    let debug_reads = std::env::var("AGEND_DEBUG_PTY_READ").is_ok();
    let mut read_count: u64 = 0;
    let mut total_bytes: u64 = 0;

    loop {
        match pty_reader.read(&mut buf) {
            Ok(0) => {
                if debug_reads {
                    eprintln!(
                        "[pty_read {name}] EOF after {read_count} reads, {total_bytes} bytes"
                    );
                }
                break;
            }
            Ok(n_bytes) => {
                if debug_reads {
                    read_count += 1;
                    total_bytes += n_bytes as u64;
                    let snip: String = buf[..n_bytes.min(64)]
                        .iter()
                        .map(|b| {
                            if b.is_ascii_graphic() || *b == b' ' {
                                (*b as char).to_string()
                            } else {
                                format!("\\x{:02x}", b)
                            }
                        })
                        .collect();
                    eprintln!(
                        "[pty_read {name}] read#{read_count} {n_bytes}B total={total_bytes} first64={snip}"
                    );
                }
                let data = &buf[..n_bytes];

                capture.write(data);

                // Feed VTerm + state detection + broadcast (under same lock = atomic),
                // then scan the rendered screen for dismiss patterns. Scanning
                // post-render means we match what the user actually sees —
                // Ink-style TUIs that draw char-by-char with cursor positioning
                // won't defeat us (VTerm resolves the geometry). Cooldown: 10s.
                let screen = {
                    let mut c = core.lock();
                    // #919: push the raw PTY bytes (still containing
                    // ANSI escapes) into the state tracker's anchor
                    // ring BEFORE `vterm.process` consumes / strips
                    // them. Used by the red-SGR anchor gate to
                    // discriminate real backend errors from prose
                    // copied via inject_to_agent (which strips ANSI).
                    c.state.feed_raw(data);
                    c.vterm.process(data);
                    let rows = c.vterm.rows() as usize;
                    let screen = c.vterm.tail_lines(rows);
                    c.state.feed(&screen);
                    c.subscribers.retain(|tx| tx.send(data.to_vec()).is_ok());
                    screen
                };

                let in_cooldown = dismiss_cooldown_until
                    .map(|t| std::time::Instant::now() < t)
                    .unwrap_or(false);
                if !in_cooldown && try_dismiss_dialog(name, &screen, pty_writer, dismiss_patterns) {
                    dismiss_cooldown_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
                }
            }
            Err(e) => {
                if debug_reads {
                    eprintln!(
                        "[pty_read {name}] ERR after {read_count} reads, {total_bytes} bytes: {e}"
                    );
                }
                tracing::warn!(agent = name, error = %e, "PTY read error, triggering cleanup");
                break;
            }
        }
    }

    // #1144: handle_pty_close runs after BOTH exit paths (EOF and read error).
    // Previously only the Ok(0) branch called it; the Err branch broke without
    // cleanup, leaving a zombie agent in the registry.
    handle_pty_close(name, registry, home, crash_tx, shutdown, deleted);
}

/// Sprint 21 F-NEW1: kill the process tree rooted at the agent's child PID,
/// if still registered. Looks up the PID through the registry → child mutex
/// chain, then delegates to [`crate::process::kill_process_tree`] (PR-U #158).
///
/// No-op if the agent is not in the registry (already cleaned up) or if the
/// child has no live PID (already reaped). Idempotent on dead PIDs — safe to
/// call multiple times during the same crash-detection sequence.
fn sweep_child_tree(name: &str, registry: &AgentRegistry) {
    let pid: Option<u32> = {
        let reg = registry.lock();
        reg.get(name).and_then(|h| h.child.lock().process_id())
    };
    if let Some(pid) = pid {
        crate::process::kill_process_tree(pid);
    }
}

/// Handle PTY close: determine if crash, graceful exit, or daemon shutdown.
/// Exit classification for handle_pty_close dispatch.
enum ExitKind {
    UserExit,
    SignalKill,
    Crash,
}

fn classify_exit(exit_code: Option<i32>) -> ExitKind {
    match exit_code {
        Some(0) | Some(130) => ExitKind::UserExit,
        Some(137) | Some(143) => {
            tracing::info!(
                exit_code = exit_code.unwrap_or(0),
                "killed by signal, not crash"
            );
            ExitKind::SignalKill
        }
        Some(c) => {
            tracing::warn!(exit_code = c, "crash");
            ExitKind::Crash
        }
        None => {
            tracing::warn!("process didn't exit in 2s, treating as crash");
            ExitKind::Crash
        }
    }
}

fn wait_for_process_exit(name: &str, registry: &AgentRegistry) -> Option<i32> {
    for _ in 0..20 {
        let reg = registry.lock();
        if reg.get(name).is_none() {
            tracing::debug!(agent = name, "not in registry, skipping crash handling");
            return Some(0);
        }
        if let Some(handle) = reg.get(name) {
            let mut c = handle.child.lock();
            if let Ok(Some(status)) = c.try_wait() {
                return Some(status.exit_code() as i32);
            }
        }
        drop(reg);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

fn cleanup_agent(name: &str, registry: &AgentRegistry, home: &Option<std::path::PathBuf>) {
    registry.lock().remove(name);
    if let Some(ref home) = home {
        crate::ipc::remove_port(&crate::daemon::run_dir(home), name);
    }
}

fn is_startup_failure(name: &str, registry: &AgentRegistry) -> bool {
    let uptime = {
        let reg = registry.lock();
        reg.get(name)
            .map(|h| (h.spawned_at.elapsed(), h.spawned_at_epoch_ms))
    };
    let had_user_input = {
        let pair = crate::daemon::heartbeat_pair::snapshot_for(name);
        uptime
            .map(|(_, epoch_ms)| pair.last_input_at_ms >= epoch_ms)
            .unwrap_or(false)
    };
    matches!(uptime, Some((d, _)) if d < std::time::Duration::from_secs(5) && !had_user_input)
}

fn on_startup_failure(
    name: &str,
    home: &Option<std::path::PathBuf>,
    crash_tx: &Option<CrashChannel>,
) {
    tracing::warn!(
        agent = name,
        "startup failure (exited too quickly, no user input)"
    );
    if let Some(ref home) = home {
        crate::event_log::log(
            home,
            "startup_failure",
            name,
            "exited too quickly, no user input",
        );
    }
    if let Some(ref tx) = crash_tx {
        let _ = tx.send(AgentExitEvent::Crash(name.to_string()));
    }
}

fn on_clean_exit_shell_fallback(
    name: &str,
    exit_code: Option<i32>,
    registry: &AgentRegistry,
    home: &Option<std::path::PathBuf>,
    crash_tx: &Option<CrashChannel>,
    shutdown: &Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    tracing::info!(
        agent = name,
        ?exit_code,
        "clean exit, spawning shell fallback"
    );
    if let Some(ref home) = home {
        crate::event_log::log(home, "clean_exit", name, "agent exited cleanly");
    }

    let (cols, rows) = {
        let reg = registry.lock();
        reg.get(name)
            .map(|h| {
                let c = h.core.lock();
                (c.vterm.cols(), c.vterm.rows())
            })
            .unwrap_or_else(|| crossterm::terminal::size().unwrap_or((120, 40)))
    };

    let work_dir: Option<std::path::PathBuf> = home.as_ref().and_then(|h| {
        let meta_path = h.join("metadata").join(format!("{name}.json"));
        std::fs::read_to_string(meta_path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|v| {
                v["working_directory"]
                    .as_str()
                    .map(std::path::PathBuf::from)
            })
    });

    cleanup_agent(name, registry, home);

    let shell = crate::default_shell();
    let spawn_result = spawn_agent(
        &SpawnConfig {
            name,
            backend_command: shell,
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols,
            rows,
            env: None,
            working_dir: work_dir.as_deref(),
            submit_key: "\r",
            home: home.as_deref(),
            crash_tx: crash_tx.clone(),
            shutdown: shutdown.clone(),
        },
        registry,
    );
    match spawn_result {
        Ok(()) => {
            tracing::info!(agent = name, shell, "shell fallback spawned");
            if let Some(ref home) = home {
                let rdir = crate::daemon::run_dir(home);
                let reg = Arc::clone(registry);
                let n = name.to_string();
                // fire-and-forget: shell TUI server exits when agent removed.
                let _ = std::thread::Builder::new()
                    .name(format!("{n}_tui"))
                    .spawn(move || crate::daemon::serve_agent_tui(&n, &rdir, &reg));
            }
        }
        Err(e) => {
            tracing::warn!(agent = name, error = %e, "shell fallback failed");
            if let Some(ref home) = home {
                let _ = crate::api::call(
                    home,
                    &serde_json::json!({"method": crate::api::method::DELETE, "params": {"name": name}}),
                );
            }
            if let Some(ref tx) = crash_tx {
                let _ = tx.try_send(AgentExitEvent::CleanExit(name.to_string()));
            }
        }
    }
}

fn on_crash_exit(name: &str, registry: &AgentRegistry, crash_tx: &Option<CrashChannel>) {
    tracing::info!(agent = name, "setting restarting state");
    {
        let reg = registry.lock();
        if let Some(handle) = reg.get(name) {
            handle.core.lock().state.set_restarting();
        }
    }
    if let Some(ref tx) = crash_tx {
        if let Err(e) = tx.try_send(AgentExitEvent::Crash(name.to_string())) {
            tracing::warn!(agent = %name, error = %e, "crash channel full — respawn event dropped");
        }
    }
}

fn handle_pty_close(
    name: &str,
    registry: &AgentRegistry,
    home: &Option<std::path::PathBuf>,
    crash_tx: &Option<CrashChannel>,
    shutdown: &Option<Arc<std::sync::atomic::AtomicBool>>,
    deleted: &Arc<std::sync::atomic::AtomicBool>,
) {
    let is_shutdown = shutdown
        .as_ref()
        .map(|s| s.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(false);
    if is_shutdown {
        tracing::info!(agent = name, "stopped (daemon shutdown)");
        cleanup_agent(name, registry, home);
        return;
    }

    tracing::info!(agent = name, "PTY closed, waiting for process exit");
    let exit_code = wait_for_process_exit(name, registry);
    sweep_child_tree(name, registry);

    if deleted.load(std::sync::atomic::Ordering::SeqCst) {
        tracing::info!(agent = name, "agent deleted, skipping shell fallback");
        return;
    }

    match classify_exit(exit_code) {
        ExitKind::UserExit => {
            if is_startup_failure(name, registry) {
                on_startup_failure(name, home, crash_tx);
            } else {
                on_clean_exit_shell_fallback(name, exit_code, registry, home, crash_tx, shutdown);
            }
        }
        ExitKind::SignalKill => {
            cleanup_agent(name, registry, home);
        }
        ExitKind::Crash => {
            on_crash_exit(name, registry, crash_tx);
        }
    }
}

/// Write data to an agent's PTY (atomic write — for attach path).
/// PTY write timeout. Prevents indefinite blocking when backend stops
/// reading stdin (buffer full). Spawns a short-lived thread for the write;
/// if it doesn't complete within the timeout, returns TimedOut error.
/// Uses an AtomicBool guard to prevent thread accumulation: if a previous
/// write is still stuck, returns TimedOut immediately without spawning.
const PTY_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Per-writer in-progress guard. Keyed by Arc identity (pointer address).
static WRITE_IN_PROGRESS: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashSet<usize>>,
> = std::sync::OnceLock::new();

fn write_in_progress_set() -> &'static parking_lot::Mutex<std::collections::HashSet<usize>> {
    WRITE_IN_PROGRESS.get_or_init(|| parking_lot::Mutex::new(std::collections::HashSet::new()))
}

fn write_with_timeout(writer: &PtyWriter, data: &[u8]) -> std::io::Result<()> {
    let key = Arc::as_ptr(writer) as usize;

    // If a previous write is still stuck, fail fast.
    {
        let set = write_in_progress_set();
        let mut guard = set.lock();
        if guard.contains(&key) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "PTY write already in progress (previous write stuck)",
            ));
        }
        guard.insert(key);
    }

    let data = data.to_vec();
    let writer = Arc::clone(writer);
    let (tx, rx) = crossbeam_channel::bounded(1);
    // #1145: move `key` into thread so it can clear the WRITE_IN_PROGRESS guard
    // on exit — even if the caller already timed out. Without this, a timeout
    // leaves the guard set permanently, blocking all future writes to this
    // PtyWriter (and any new writer allocated at the same address after teardown).
    // fire-and-forget: write thread is short-lived (bounded by PTY buffer drain);
    // on timeout the caller returns TimedOut but the thread eventually completes
    // and self-cleans the guard.
    let spawn_result = std::thread::Builder::new()
        .name("pty_write_timeout".into())
        .spawn(move || {
            let result = (|| {
                let mut w = writer.lock();
                w.write_all(&data)?;
                w.flush()
            })();
            // Thread-side guard cleanup: if the caller timed out, rx is dropped
            // and send returns Err — but we still clear the guard so the next
            // write attempt is not permanently blocked.
            write_in_progress_set().lock().remove(&key);
            let _ = tx.send(result);
        });
    if let Err(e) = spawn_result {
        write_in_progress_set().lock().remove(&key);
        return Err(std::io::Error::other(format!(
            "PTY write thread spawn failed: {e}"
        )));
    }
    let result = match rx.recv_timeout(PTY_WRITE_TIMEOUT) {
        Ok(Ok(())) => {
            // Success: clear guard
            write_in_progress_set().lock().remove(&key);
            Ok(())
        }
        Ok(Err(e)) => {
            // Fast failure (BrokenPipe etc): clear guard, allow retry
            write_in_progress_set().lock().remove(&key);
            Err(e)
        }
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
            // Thread still running: guard will be cleared by the thread itself
            // when it eventually completes (#1145).
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "PTY write timed out (5s) — backend may be stuck",
            ))
        }
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
            // Thread panicked or dropped: clear guard
            write_in_progress_set().lock().remove(&key);
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "PTY write thread disconnected",
            ))
        }
    };

    result
}

pub fn write_to_agent(agent: &AgentHandle, data: &[u8]) -> crate::error::Result<()> {
    write_with_timeout(&agent.pty_writer, data).map_err(crate::error::AgendError::PtyWrite)?;
    Ok(())
}

/// Write data to an agent's PTY byte-by-byte with small delays.
#[allow(dead_code)]
pub fn write_to_agent_typed(agent: &AgentHandle, data: &[u8]) -> crate::error::Result<()> {
    for byte in data {
        write_with_timeout(&agent.pty_writer, &[*byte])?;
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    Ok(())
}

/// Inject text + submit to agent PTY. Splits text from submit_key with a delay
/// so TUI frameworks process them as separate events.
/// - typed=false: write_all(prefix+text), delay, write_all(submit_key)
/// - typed=true: per-byte(prefix+text), delay, write_all(submit_key)
pub fn inject_to_agent(agent: &AgentHandle, text: &[u8]) -> crate::error::Result<()> {
    let target = InjectTarget::from_handle(agent);
    inject_with_target(&target, text)
}

/// #1146: inner inject that works on a snapshot of fields rather than a
/// borrowed `AgentHandle`. This lets callers release the registry lock
/// before the slow typed-inject sleep loop runs.
fn inject_with_target(target: &InjectTarget, text: &[u8]) -> crate::error::Result<()> {
    if target.deleted.load(std::sync::atomic::Ordering::Acquire) {
        return Ok(());
    }
    let prefix = target.inject_prefix.as_bytes();
    let submit = target.submit_key.as_bytes();

    // S54 fix: strip ANSI sequences before injection to avoid ESC conflict in typed_inject.
    let text_str = String::from_utf8_lossy(text);
    let stripped = strip_ansi(&text_str);
    let text_bytes = stripped.as_bytes();

    if target.typed_inject {
        let all_bytes: Vec<u8> = prefix.iter().chain(text_bytes.iter()).copied().collect();

        // Issue #658: system headers must be written atomically.
        let is_system_header = stripped.starts_with(crate::inbox::SYSTEM_MSG_PREFIX)
            || stripped.starts_with(crate::inbox::AGENT_MSG_PREFIX);
        let (atomic_part, chunk_part) = if is_system_header {
            match all_bytes.iter().position(|&b| b == b'\n') {
                Some(pos) => all_bytes.split_at(pos + 1),
                None => (all_bytes.as_slice(), &[] as &[u8]),
            }
        } else {
            (&[] as &[u8], all_bytes.as_slice())
        };

        if !atomic_part.is_empty() {
            write_with_timeout(&target.pty_writer, atomic_part)?;
            std::thread::sleep(std::time::Duration::from_millis(
                2 * atomic_part.len() as u64,
            ));
        }

        for chunk in chunk_part.chunks(64) {
            if target.deleted.load(std::sync::atomic::Ordering::Acquire) {
                return Ok(());
            }
            write_with_timeout(&target.pty_writer, chunk)?;
            std::thread::sleep(std::time::Duration::from_millis(2 * chunk.len() as u64));
        }
    } else {
        let mut combined = Vec::with_capacity(prefix.len() + text_bytes.len());
        combined.extend_from_slice(prefix);
        combined.extend_from_slice(text_bytes);
        write_with_timeout(&target.pty_writer, &combined)?;
    }

    if target.deleted.load(std::sync::atomic::Ordering::Acquire) {
        return Ok(());
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    write_with_timeout(&target.pty_writer, submit)?;
    Ok(())
}

/// #1146: lightweight clone of the fields `inject_to_agent` reads from
/// `AgentHandle`. Lets callers snapshot under lock then inject after
/// releasing the registry mutex — typed_inject agents sleep 2ms per
/// chunk (10KB ≈ 20s), so holding the registry lock during inject
/// blocks every other registry operation for the entire duration.
#[derive(Clone)]
pub(crate) struct InjectTarget {
    pub pty_writer: PtyWriter,
    pub inject_prefix: String,
    pub submit_key: String,
    pub typed_inject: bool,
    pub deleted: Arc<std::sync::atomic::AtomicBool>,
}

impl InjectTarget {
    pub fn from_handle(h: &AgentHandle) -> Self {
        Self {
            pty_writer: Arc::clone(&h.pty_writer),
            inject_prefix: h.inject_prefix.clone(),
            submit_key: h.submit_key.clone(),
            typed_inject: h.typed_inject,
            deleted: Arc::clone(&h.deleted),
        }
    }
}

/// Send a message to a named agent via direct registry injection.
/// Returns true if the agent was found and injected.
pub fn send_to_registry(registry: &AgentRegistry, from: &str, target: &str, text: &str) -> bool {
    let target_snapshot = {
        let reg = lock_registry(registry);
        match reg.get(target) {
            Some(handle) => InjectTarget::from_handle(handle),
            None => return false,
        }
    }; // lock released before inject
    let msg = format!("[from:{from}] {text}");
    let _ = inject_with_target(&target_snapshot, msg.as_bytes());
    true
}

/// Broadcast a message to all agents with recognized backends.
/// Skips `exclude` (typically the sender) if provided.
pub fn broadcast_registry(
    registry: &AgentRegistry,
    from: &str,
    text: &str,
    exclude: Option<&str>,
) -> Vec<String> {
    let msg = format!("[from:{from}] {text}");
    let msg_bytes = msg.as_bytes();
    // #1146: snapshot names + inject targets under one lock, release,
    // then inject without holding the registry. Previous code re-acquired
    // the lock per-target and held it during inject — typed_inject agents
    // sleep 2ms/chunk, so N targets × 20s blocked the entire registry.
    let targets: Vec<(String, InjectTarget)> = {
        let reg = lock_registry(registry);
        reg.iter()
            .filter(|(name, handle)| {
                (exclude != Some(name.as_str()))
                    && crate::backend::Backend::from_command(&handle.backend_command).is_some()
            })
            .map(|(name, handle)| (name.clone(), InjectTarget::from_handle(handle)))
            .collect()
    }; // lock released before any inject
    let target_names: Vec<String> = targets.iter().map(|(n, _)| n.clone()).collect();
    for (_, snapshot) in &targets {
        let _ = inject_with_target(snapshot, msg_bytes);
    }
    target_names
}

/// Get atomic subscribe + screen dump (under core lock — no output gap).
/// Creates a new per-subscriber channel. Each subscriber gets ALL output (broadcast).
pub fn subscribe_with_dump(agent: &AgentHandle) -> (crossbeam_channel::Receiver<Vec<u8>>, Vec<u8>) {
    let mut core = agent.core.lock();
    let dump = core.vterm.dump_screen();
    let (tx, rx) = crossbeam_channel::bounded(1024);
    core.subscribers.push(tx);
    (rx, dump)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
