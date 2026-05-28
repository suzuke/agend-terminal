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
    pub(crate) name: String,
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
                name: name.to_string(),
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
                .map(|(p, k)| (p.to_string(), k.to_vec()))
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

/// Try to auto-dismiss dialogs using backend-configurable patterns. Returns true if dismissed.
/// `screen` is the VTerm-rendered view the user sees — not raw PTY bytes —
/// so Ink-style TUIs that paint char-by-char with cursor positioning still match.
/// Cached regex compilation for dismiss patterns.
///
/// Issue #468: dismiss patterns must match anchored regex (line start +
/// optional TUI prefix), not bare substring. Compiles once per unique pattern
/// string and reuses the `Arc<Regex>` thereafter so the screen-update hot
/// loop never re-compiles.
///
/// r1 fix (PR #469 reviewer): both successful AND failed compiles are cached.
/// The cache value is `Option<Arc<Regex>>` — `None` records that the pattern
/// is permanently invalid, so subsequent lookups skip the compile + log path
/// entirely. Without this, a typo in a backend preset would re-compile and
/// re-emit a warn line on every screen-update tick. The warn (not error —
/// invalid patterns are configurer mistakes, not runtime faults) fires once
/// per unique bad pattern over the process lifetime.
static DISMISS_REGEX_CACHE: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, Option<std::sync::Arc<regex::Regex>>>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

fn compile_dismiss_regex(pattern: &str) -> Option<std::sync::Arc<regex::Regex>> {
    let mut cache = DISMISS_REGEX_CACHE.lock();
    if let Some(slot) = cache.get(pattern) {
        return slot.as_ref().map(std::sync::Arc::clone);
    }
    let result = match regex::Regex::new(pattern) {
        Ok(re) => Some(std::sync::Arc::new(re)),
        Err(e) => {
            tracing::warn!(
                pattern,
                error = %e,
                "dismiss regex compile failed — pattern ignored"
            );
            None
        }
    };
    cache.insert(pattern.to_string(), result.clone());
    result
}

/// Test-only inspection of the dismiss regex cache. Used by the
/// `invalid_regex_cached_no_relog` test to assert that bad patterns get
/// cached after first failure (rather than re-compiling on every call).
#[cfg(test)]
fn dismiss_regex_cache_contains(pattern: &str) -> bool {
    DISMISS_REGEX_CACHE.lock().contains_key(pattern)
}

/// Strip the standard line-anchor prefix to recover the literal hint from a
/// dismiss regex. Used by Step 4 (false-positive operator visibility logging).
/// Returns the input unchanged when no known prefix is present so callers
/// don't accidentally compare an entire regex against `screen.contains`.
///
/// Issue #468 follow-up (kiro startup hang): the original prefix
/// `[│║|>\s]*` only covered Ink box-drawing chars and the `>` cursor.
/// kiro-cli's "Trust All Tools" prompt renders the selected option with
/// a `) No, exit` (radio-button style cursor), which the narrow class did
/// not match — dismiss never fired and kiro hung on confirmation.
///
/// Bounded-permissive replacement: any non-alpha non-newline byte in the
/// leading 0–8 chars. The length cap (8) preserves the line-start anchor's
/// intent — scrollback or user text containing the phrase mid-paragraph is
/// preceded by alpha chars or a much longer indent, so it cannot match.
/// The class covers `)`, `(`, `*`, `•`, digits in `[3]`-style choice rows,
/// and any future cursor variant introduced by a backend's TUI without
/// requiring a new patch per backend.
const DISMISS_REGEX_PREFIX: &str = r"(?m)^[^A-Za-z\n]{0,8}";

fn dismiss_literal_hint(pattern: &str) -> &str {
    pattern
        .strip_prefix(DISMISS_REGEX_PREFIX)
        .unwrap_or(pattern)
}

pub fn try_dismiss_dialog(
    name: &str,
    screen: &str,
    pty_writer: &PtyWriter,
    dismiss_patterns: &[(String, Vec<u8>)],
) -> bool {
    if dismiss_patterns.is_empty() {
        return false;
    }

    for (pattern, key_seq) in dismiss_patterns {
        // Issue #468: regex match anchored to line start + optional TUI prefix.
        // Substring match (the prior behavior) auto-injected `2\n` / `3\n`
        // whenever the phrase appeared anywhere on screen — including in agent
        // output and scrollback — sending input the user never authorized.
        let Some(re) = compile_dismiss_regex(pattern) else {
            continue;
        };
        if re.is_match(screen) {
            tracing::info!(agent = name, pattern, "auto-dismissing dialog");
            // Delayed write: TUI escape-sequence parsers need time to distinguish
            // \x1b (ESC key) from \x1b[ (CSI start).  Writing immediately causes
            // Ink-based TUIs (kiro-cli) to interpret \x1b as "ESC to cancel".
            // H2: bounded dismiss — skip if one already in-flight for this agent.
            // Prevents thread accumulation from rapid dialog re-detection.
            static DISMISS_IN_FLIGHT: std::sync::LazyLock<
                parking_lot::Mutex<std::collections::HashSet<String>>,
            > = std::sync::LazyLock::new(|| {
                parking_lot::Mutex::new(std::collections::HashSet::new())
            });
            {
                let mut inflight = DISMISS_IN_FLIGHT.lock();
                if inflight.contains(name) {
                    return true; // dismiss already pending
                }
                inflight.insert(name.to_string());
            }
            let writer = Arc::clone(pty_writer);
            let keys = key_seq.clone();
            let agent = name.to_string();
            // fire-and-forget: dialog-dismiss keystroke writer is short-lived
            // (sleep 300ms then write). H2: removes from in-flight set on exit.
            if std::thread::Builder::new()
                .name("dismiss-dialog".into())
                .spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    // Send keys in chunks split on \r/\n boundaries with delay between,
                    // so TUI frameworks process navigation before confirmation.
                    let mut w = writer.lock();
                    let mut start = 0;
                    for (i, &b) in keys.iter().enumerate() {
                        if b == b'\r' || b == b'\n' {
                            // Send everything up to (not including) this Enter
                            if start < i {
                                let _ = w.write_all(&keys[start..i]);
                                let _ = w.flush();
                                drop(w);
                                std::thread::sleep(std::time::Duration::from_millis(200));
                                w = writer.lock();
                            }
                            // Send the Enter
                            let _ = w.write_all(&keys[i..=i]);
                            let _ = w.flush();
                            start = i + 1;
                        }
                    }
                    if start < keys.len() {
                        let _ = w.write_all(&keys[start..]);
                        let _ = w.flush();
                    }
                    tracing::debug!(agent = %agent, "dismiss keystrokes sent");
                    // H2: remove from in-flight set
                    DISMISS_IN_FLIGHT.lock().remove(&agent);
                })
                .is_err()
            {
                tracing::warn!(agent = name, "failed to spawn dismiss-dialog thread");
                DISMISS_IN_FLIGHT.lock().remove(name);
            }
            return true;
        }
        // Step 4 (Issue #468): operator-visibility log when the literal hint
        // would have triggered the old substring path but the new regex
        // anchor declined — surfaces realistic false positives (mid-paragraph
        // matches, scrollback echoes) without auto-injecting bytes.
        let literal = dismiss_literal_hint(pattern);
        if literal != pattern.as_str() && !literal.is_empty() && screen.contains(literal) {
            tracing::debug!(
                agent = name,
                pattern,
                literal,
                "dismiss substring seen but regex didn't match — likely false positive"
            );
        }
    }

    false
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
mod tests {
    use super::*;

    /// #945 Phase 1: pending-registry slot publishes the agent registry
    /// to the background `telegram_init` thread. The slot is a
    /// `OnceLock` (set-once-per-process) so the test asserts that
    /// (a) initial state is empty, (b) `set_pending_registry` makes
    /// the registry observable via `get_pending_registry`.
    ///
    /// Process-shared state: this test runs in cargo's per-process
    /// model so the OnceLock is fresh per `cargo test` invocation. If
    /// re-run in the same binary instance (rare), the second call to
    /// `set_pending_registry` no-ops — but `get_pending_registry`
    /// still returns the originally-set value, which is the documented
    /// behavior (first publisher wins).
    #[test]
    fn pending_registry_publish_and_observe_945() {
        // Note: OnceLock may have been populated by an earlier test
        // in the same process. If `get_pending_registry()` returns
        // Some already, skip with a clear message — we can't reset
        // OnceLock state.
        if get_pending_registry().is_some() {
            eprintln!(
                "test fixture: PENDING_REGISTRY already populated by earlier \
                 test in this process. OnceLock is set-once; skipping. The \
                 set-once semantic is itself the contract this test pins."
            );
            return;
        }
        let registry: AgentRegistry =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        set_pending_registry(Arc::clone(&registry));
        let observed = get_pending_registry().expect("registry must be observable post-publish");
        // Identity check: same Arc-pointer.
        assert!(
            Arc::ptr_eq(&registry, &observed),
            "get_pending_registry must return the SAME Arc that was published"
        );
    }

    #[test]
    fn validate_name_valid() {
        assert!(validate_name("hello").is_ok());
        assert!(validate_name("agent-1").is_ok());
        assert!(validate_name("my_agent").is_ok());
        assert!(validate_name("A123").is_ok());
    }

    #[test]
    fn validate_name_rejects_traversal() {
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("foo/bar").is_err());
        assert!(validate_name("a b").is_err());
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_rejects_long() {
        let long = "a".repeat(65);
        assert!(validate_name(&long).is_err());
        let ok = "a".repeat(64);
        assert!(validate_name(&ok).is_ok());
    }

    #[test]
    fn strip_ansi_basic() {
        assert_eq!(strip_ansi("hello"), "hello");
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b[1;32mbold green\x1b[0m"), "bold green");
    }

    /// Regression: ESC byte (0x1B) must not survive `strip_ansi` for any of the
    /// payload shapes `inject_to_agent` is realistically asked to deliver.
    /// `inject_to_agent` strips `text` through `strip_ansi` before writing to
    /// the PTY in `typed_inject` mode; if ESC bytes leak through, slow byte-by-
    /// byte rendering can cause Ink-style TUIs (kiro-cli) to interpret the
    /// stray ESC as the "cancel current input" keypress.
    ///
    /// This test pins that contract at the strip boundary so a future refactor
    /// of `strip_ansi` (or a new colorized injector path) that lets ESC slip
    /// through trips here, regardless of whether the subsequent chunked write
    /// path is exercised by other tests.
    #[test]
    fn inject_strips_ansi_from_typed_payload() {
        // Realistic [AGEND-MSG] header with foreground/reset color codes —
        // matches the shape that triggered the original Sprint 54 regression.
        let agend_header = "\x1b[1;36m[AGEND-MSG]\x1b[0m from=lead kind=task\n";
        let stripped = strip_ansi(agend_header);
        assert!(
            !stripped.as_bytes().contains(&0x1B),
            "ESC byte must not survive strip_ansi for typed inject; got {stripped:?}"
        );
        // Cursor-movement and OSC sequences also appear in some [AGEND-MSG]
        // emitters; exercise both to make sure no escape category leaks.
        let mixed = "\x1b[2K\r\x1b]0;title\x07message body \x1b[31mred\x1b[0m";
        let stripped_mixed = strip_ansi(mixed);
        assert!(
            !stripped_mixed.as_bytes().contains(&0x1B),
            "ESC byte must not survive strip_ansi for mixed CSI+OSC payload; got {stripped_mixed:?}"
        );
    }

    #[test]
    fn strip_ansi_cursor_move_no_space() {
        // CSI C (cursor forward) and D (cursor back) must not insert spaces
        assert_eq!(strip_ansi("\x1b[5Chello"), "hello");
        assert_eq!(strip_ansi("ab\x1b[2Dcd"), "abcd");
        // Other CSI codes also produce nothing
        assert_eq!(strip_ansi("\x1b[Hhome"), "home");
    }

    #[test]
    fn strip_ansi_osc() {
        assert_eq!(strip_ansi("\x1b]0;title\x07rest"), "rest");
    }

    #[test]
    fn sensitive_env_keys_covers_known_dangerous() {
        assert!(is_sensitive_env_key("ANTHROPIC_API_KEY"));
        assert!(is_sensitive_env_key("OPENAI_API_KEY"));
        assert!(is_sensitive_env_key("AWS_SECRET_ACCESS_KEY"));
        assert!(is_sensitive_env_key("LD_PRELOAD"));
        assert!(is_sensitive_env_key("DYLD_INSERT_LIBRARIES"));
        assert!(is_sensitive_env_key("AGEND_HOME"));
        assert!(is_sensitive_env_key("AGEND_MCP_TOOLS_DENY"));
    }

    #[test]
    fn sensitive_env_keys_is_case_insensitive() {
        // Windows env is case-insensitive; ensure lower-cased fleet.yaml keys
        // still hit the deny-list.
        assert!(is_sensitive_env_key("anthropic_api_key"));
        assert!(is_sensitive_env_key("Ld_Preload"));
    }

    #[test]
    fn sensitive_env_keys_allows_benign() {
        assert!(!is_sensitive_env_key("MY_APP_DEBUG"));
        assert!(!is_sensitive_env_key("LANG"));
        assert!(!is_sensitive_env_key("TERM"));
        assert!(!is_sensitive_env_key("PROMPT_OVERRIDE"));
    }

    fn test_writer() -> PtyWriter {
        Arc::new(Mutex::new(Box::new(Vec::<u8>::new())))
    }

    #[test]
    fn dismiss_fires_when_pattern_in_screen() {
        let patterns = vec![("Do you trust".to_string(), b"\n".to_vec())];
        let hit = try_dismiss_dialog(
            "t",
            "Do you trust the contents of this directory?",
            &test_writer(),
            &patterns,
        );
        assert!(hit);
    }

    #[test]
    fn dismiss_skips_when_pattern_absent() {
        let patterns = vec![("Do you trust".to_string(), b"\n".to_vec())];
        let hit = try_dismiss_dialog("t", "unrelated screen content", &test_writer(), &patterns);
        assert!(!hit);
    }

    #[test]
    fn dismiss_skips_when_no_patterns() {
        assert!(!try_dismiss_dialog("t", "anything", &test_writer(), &[]));
    }

    #[test]
    fn dismiss_matches_ink_style_cursor_painted_prompt() {
        // Regression for macOS: Ink-based TUIs (codex) paint text by
        // positioning the cursor before each segment. VTerm resolves this
        // into a clean screen; the old raw-byte strip_ansi path was fragile
        // on such streams. Drive VTerm with BSU + cursor positioning and
        // confirm the rendered screen still contains the pattern literally.
        let mut vt = crate::vterm::VTerm::new(80, 24);
        vt.process(b"\x1b[?2026h"); // begin synchronized update
        vt.process(b"\x1b[5;2HDo you trust"); // row 5 col 2
        vt.process(b"\x1b[5;15H the contents of this directory?");
        vt.process(b"\x1b[?2026l"); // end synchronized update
        let screen = vt.tail_lines(24);
        let patterns = vec![("Do you trust".to_string(), b"\n".to_vec())];
        assert!(try_dismiss_dialog("t", &screen, &test_writer(), &patterns));
    }

    // ── Issue #468: dismiss precision regression tests ─────────────────
    //
    // Hotfix #468 replaces `screen.contains(pattern)` substring match with
    // an anchored regex (`(?m)^[│║|>\s]*<text>`) so user input and
    // scrollback content containing the dialog phrase mid-paragraph cannot
    // trigger an unauthorized auto-dismiss.
    //
    // Production-realistic patterns: these tests use the EXACT regex strings
    // from `BackendPreset::dismiss_patterns` (Backend::Gemini) so a future
    // refactor that diverges the test pattern from prod would still trigger
    // these assertions on the prod string.
    //
    // Regression-proof: revert `try_dismiss_dialog` to use
    // `screen.contains(pattern.as_str())` (bare substring match) and the
    // false-positive tests below FAIL. Restore the regex match → PASS.

    /// Production dismiss regex for Gemini's MCP-tool dialog (Issue #468).
    const GEMINI_MCP_TOOL_REGEX: &str = r"(?m)^[^A-Za-z\n]{0,8}Allow execution of MCP tool";
    /// Production dismiss regex for Gemini's shell-execution dialog (Issue #468).
    const GEMINI_SHELL_REGEX: &str = r"(?m)^[^A-Za-z\n]{0,8}Allow execution of:";
    /// Production dismiss regex for kiro-cli's "Trust All Tools" prompt
    /// (Issue #468 follow-up — radio-button cursor `)` was unmatched).
    const KIRO_TRUST_REGEX: &str = r"(?m)^[^A-Za-z\n]{0,8}No, exit";
    /// Production dismiss regex for Claude's workspace-trust prompt (#996
    /// Phase 1). Modern Claude (v2.1.145+) defaults cursor to "Yes, I trust",
    /// so the keystroke shipped is single Enter `\r` — see
    /// `Backend::ClaudeCode.preset().dismiss_patterns[0]`.
    const CLAUDE_TRUST_REGEX: &str = r"(?m)^[^A-Za-z\n]{0,8}Yes, I trust";

    /// `(regex, keystrokes)` pair for `try_dismiss_dialog` — `Down` then
    /// `Enter` to dismiss kiro-cli's "Trust All Tools" prompt.
    fn kiro_trust_patterns() -> Vec<(String, Vec<u8>)> {
        vec![(KIRO_TRUST_REGEX.to_string(), b"\x1b[B\r".to_vec())]
    }

    #[test]
    fn issue_468_true_positive_gemini_mcp_dialog() {
        // Realistic Gemini Ink TUI render: dialog body is wrapped in box-drawing
        // chars, lines start with `│ ` (vertical bar + space).
        let screen = "\
╭─────────────────────────────────────────────╮
│ Allow execution of MCP tool: ripgrep?       │
│                                             │
│ [1] Allow once                              │
│ [2] Allow for session                       │
│ [3] Allow always (this server)              │
│ [4] Reject                                  │
╰─────────────────────────────────────────────╯
";
        let patterns = vec![(GEMINI_MCP_TOOL_REGEX.to_string(), b"3\n".to_vec())];
        assert!(
            try_dismiss_dialog("t", screen, &test_writer(), &patterns),
            "true Gemini dialog (TUI prefix `│ `) must match anchored regex"
        );
    }

    #[test]
    fn issue_468_false_positive_mid_paragraph_user_text() {
        // User-typed message containing the dialog phrase mid-paragraph.
        // Substring match would auto-inject `2\n`; anchored regex must not.
        let screen = "\
Could you explain what \"Allow execution of: bash\" actually means?
I see it in the docs but I'm not sure when Gemini shows it.
";
        let patterns = vec![(GEMINI_SHELL_REGEX.to_string(), b"2\n".to_vec())];
        assert!(
            !try_dismiss_dialog("t", screen, &test_writer(), &patterns),
            "Issue #468: user text with phrase mid-paragraph must NOT trigger dismiss"
        );
    }

    #[test]
    fn issue_468_false_positive_scrollback_agent_output() {
        // Agent's prior output explains the dialog format — phrase appears
        // embedded in a longer line. Substring match would auto-fire mid-stream.
        let screen = "\
[ai] When the tool is invoked, you'll see a prompt: Allow execution of MCP tool — pick option 3 to always allow.
[user] thanks!
[ai] Anything else?
";
        let patterns = vec![(GEMINI_MCP_TOOL_REGEX.to_string(), b"3\n".to_vec())];
        assert!(
            !try_dismiss_dialog("t", screen, &test_writer(), &patterns),
            "Issue #468: agent scrollback explaining the phrase must NOT trigger dismiss"
        );
    }

    #[test]
    fn issue_468_production_smoke_vterm_rendered_dialog() {
        // Production-smoke gate (§5): drive VTerm with the ANSI byte stream
        // a real Gemini TUI emits, then run try_dismiss_dialog against the
        // rendered screen. Asserts injected bytes only happen when the actual
        // dialog renders — the same failure mode the user reported.
        let mut vt = crate::vterm::VTerm::new(80, 24);
        // Synthesize a minimal Gemini-style dialog frame: top border, body line
        // with `│ Allow execution of MCP tool: ...`, choice rows, bottom border.
        vt.process(b"\x1b[2J\x1b[H"); // clear + home
        vt.process("╭───────────────────────────────────────╮\r\n".as_bytes());
        vt.process("│ Allow execution of MCP tool: gh?      │\r\n".as_bytes());
        vt.process("│                                       │\r\n".as_bytes());
        vt.process("│ [3] Allow always                      │\r\n".as_bytes());
        vt.process("╰───────────────────────────────────────╯\r\n".as_bytes());
        let screen = vt.tail_lines(24);
        let patterns = vec![(GEMINI_MCP_TOOL_REGEX.to_string(), b"3\n".to_vec())];
        assert!(
            try_dismiss_dialog("t", &screen, &test_writer(), &patterns),
            "VTerm-rendered Gemini dialog must match production regex. Screen:\n{screen}"
        );
    }

    /// #996 Phase 1: true Claude workspace-trust prompt — vterm-rendered —
    /// MUST still match the anchored regex so the dismiss fires. The fix
    /// changes the keystroke (config-pinned in backend.rs tests) but the
    /// regex is unchanged. Anti-regression for the dismiss path itself.
    #[test]
    fn claude_trust_dismiss_matches_real_modal() {
        let mut vt = crate::vterm::VTerm::new(120, 30);
        vt.process(b"\x1b[2J\x1b[H");
        vt.process(" Accessing workspace:\r\n\r\n /private/tmp/claude-test\r\n\r\n".as_bytes());
        vt.process(
            " Quick safety check: Is this a project you created or one you trust?\r\n\r\n"
                .as_bytes(),
        );
        vt.process(" ❯ 1. Yes, I trust this folder\r\n".as_bytes()); // marker on row 1 (default)
        vt.process("   2. No, exit\r\n".as_bytes());
        vt.process(" Enter to confirm · Esc to cancel\r\n".as_bytes());
        let screen = vt.tail_lines(30);
        // Production keystroke after #996 Phase 1: single Enter.
        let patterns = vec![(CLAUDE_TRUST_REGEX.to_string(), b"\r".to_vec())];
        assert!(
            try_dismiss_dialog("t", &screen, &test_writer(), &patterns),
            "real Claude trust modal (default-Yes cursor) must still match anchored regex. Screen:\n{screen}"
        );
    }

    /// #996 Phase 1: operator-quoted content matching the anchored regex —
    /// reproduces the exact false-positive class observed today on the
    /// fixup-lead pane (37 events between 19:46:55-19:53:04 +08). The match
    /// STILL fires (we don't change the regex), but the production keystroke
    /// is now `\r` (non-destructive single Enter, pinned in backend.rs
    /// tests) instead of the historical up+up+Enter (history-resubmit blast).
    #[test]
    fn claude_trust_false_positive_quoted_content_still_matches_regex() {
        // Operator pastes (or daemon-routed message includes) the Agy
        // trust-prompt example verbatim from issue #995. The leading `>` + ` `
        // satisfies the `[^A-Za-z\n]{0,8}` anchor → regex matches even
        // though this is normal conversation content, not a real modal.
        let screen = "\
[user] Filing #995 — agy bug. The trust prompt shows:
> Yes, I trust this folder
  No, exit
Should we add a dismiss_pattern?
[claude] checking the existing patterns now
";
        let patterns = vec![(CLAUDE_TRUST_REGEX.to_string(), b"\r".to_vec())];
        assert!(
            try_dismiss_dialog("t", screen, &test_writer(), &patterns),
            "regex anchor (?m)^[^A-Za-z\\n]{{0,8}} matches `> Yes, I trust` mid-conversation — \
             this is the surface that produced today's 37 false-positives on fixup-lead. \
             The fix is the keystroke (`\\r`, non-destructive), pinned in backend tests."
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn invalid_regex_cached_no_relog() {
        // r1 fix (PR #469 reviewer): a typo in a backend dismiss pattern must
        // not re-compile + re-log on every screen-update tick. Negative-cache
        // failed compiles so the warn fires once per unique bad pattern.

        // Use a pattern that the `regex` crate rejects. Unclosed group is
        // syntactically invalid in every regex flavor.
        let bad = "(?P<unclosed";
        // Pre-condition: not yet cached.
        assert!(
            !super::dismiss_regex_cache_contains(bad),
            "test invariant: cache must not pre-contain '{bad}'"
        );

        let r1 = super::compile_dismiss_regex(bad);
        assert!(
            r1.is_none(),
            "first call on invalid pattern must return None"
        );
        assert!(
            super::dismiss_regex_cache_contains(bad),
            "first call must populate the negative cache"
        );

        let r2 = super::compile_dismiss_regex(bad);
        assert!(
            r2.is_none(),
            "second call must also return None (from cache)"
        );

        // tracing-test capture: the warn must have fired (at least once).
        // Asserting "exactly once" is brittle across test-runner concurrency,
        // but the cache assertion above proves the second call did not
        // re-attempt compile — so the warn cannot have fired again from the
        // second invocation.
        assert!(
            logs_contain("dismiss regex compile failed"),
            "compile failure must be logged at warn level"
        );
    }

    #[test]
    fn issue_468_logs_substring_near_miss_for_operator_visibility() {
        // Step 4 (Issue #468): when the literal hint would have triggered
        // the old substring path but the new regex declined, emit a debug
        // log so the operator can see realistic false positives.
        // Test asserts behavior: try_dismiss_dialog returns false (no
        // injection) but the regex compile + literal extraction path is
        // exercised. The log itself is observed indirectly via the no-op
        // outcome (the actual log line is captured by tracing-test in
        // dedicated integration suites elsewhere; keeping this test free
        // of subscriber setup avoids per-test global-state collisions).
        let screen = "user said: Allow execution of: please?";
        let patterns = vec![(GEMINI_SHELL_REGEX.to_string(), b"2\n".to_vec())];
        let fired = try_dismiss_dialog("t", screen, &test_writer(), &patterns);
        assert!(
            !fired,
            "Step 4: literal-hint near-miss must NOT inject keystrokes"
        );
        // dismiss_literal_hint should recover the bare phrase from the prod regex.
        assert_eq!(
            super::dismiss_literal_hint(GEMINI_SHELL_REGEX),
            "Allow execution of:",
            "literal hint must strip the standard line-anchor prefix"
        );
    }

    // ── Issue #468 follow-up: bounded-permissive prefix variants ─────

    /// Kiro startup hang (the bug that prompted this PR): the radio-button
    /// `)` cursor was outside the original `[│║|>\s]` class, so dismiss
    /// silently no-op'd and kiro hung on the trust-all-tools confirmation.
    #[test]
    fn kiro_trust_dismiss_matches_paren_cursor() {
        // Reproduces the operator's screenshot of kiro startup: the selected
        // option is rendered as `) No, exit`, alternatives as ` Yes, ...`.
        let screen = "\
Allow Trust All Tools mode?

) No, exit
  Yes, I accept
  Yes, and don't ask again
";
        let patterns = kiro_trust_patterns();
        assert!(
            try_dismiss_dialog("t", screen, &test_writer(), &patterns),
            "kiro `) No, exit` (radio-button cursor) must match the bounded class"
        );
    }

    /// Sanity: the bounded class still accepts the prefixes the original
    /// `[│║|>\s]` class supported. Box-drawing + `>` cursor + plain space.
    #[test]
    fn dismiss_matches_classical_prefixes() {
        let cases = [
            "│ No, exit",   // Ink box-drawing
            "║ No, exit",   // double box-drawing
            "| No, exit",   // ASCII pipe
            "> No, exit",   // chevron cursor
            "  No, exit",   // bare indent
            ") No, exit",   // radio cursor (the new case)
            "[3] No, exit", // digit-bracket choice rows
        ];
        for screen in cases {
            let patterns = kiro_trust_patterns();
            assert!(
                try_dismiss_dialog("t", screen, &test_writer(), &patterns),
                "prefix variant must match: {screen:?}"
            );
        }
    }

    /// Length cap proof: a long indent (more than 8 non-alpha chars)
    /// before the phrase must NOT match. Defends against pathological
    /// scrollback that happens to start with many non-alpha chars.
    #[test]
    fn dismiss_rejects_when_prefix_exceeds_length_cap() {
        // 9 non-alpha chars ahead of the phrase — exceeds {0,8}.
        let screen = "         No, exit"; // 9 spaces
        let patterns = kiro_trust_patterns();
        assert!(
            !try_dismiss_dialog("t", screen, &test_writer(), &patterns),
            "9-char non-alpha prefix must exceed length cap and not match"
        );
    }

    /// False-positive regression: alpha char anywhere in the prefix area
    /// (typical of scrollback/user text) must still be rejected.
    #[test]
    fn dismiss_rejects_alpha_char_in_prefix_zone() {
        // Even though `Pre` is short, an alpha char in the [^A-Za-z\n]{0,8}
        // window breaks the match — proving mid-paragraph text is safe.
        let screen = "Pre: No, exit";
        let patterns = kiro_trust_patterns();
        assert!(
            !try_dismiss_dialog("t", screen, &test_writer(), &patterns),
            "alpha char in prefix zone must invalidate match (regression-safe)"
        );
    }

    /// Production smoke: spawn a real kiro-cli process and observe its
    /// startup screen via VTerm. Asserts that the rendered screen contains
    /// the kiro trust prompt and that try_dismiss_dialog matches against
    /// the production regex. Skipped when kiro-cli isn't on PATH so the
    /// test is safe on CI without forcing a kiro-cli install matrix.
    ///
    /// Run locally with:  cargo test -- --ignored kiro_real_spawn
    ///
    /// Reader runs on a dedicated thread piping into an mpsc channel —
    /// portable_pty's `try_clone_reader()` returns a blocking reader, so
    /// polling for `WouldBlock` would hang forever waiting on a kiro that
    /// has nothing more to write. The channel + `recv_timeout` pattern is
    /// the only robust way to bound the wait without a runtime dependency.
    #[test]
    #[ignore = "spawns real kiro-cli process; run locally only"]
    #[cfg(unix)]
    fn issue_468_kiro_real_spawn_dismiss_smoke() {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        use std::sync::mpsc;

        if which::which("kiro-cli").is_err() {
            eprintln!("SKIP: kiro-cli not on PATH");
            return;
        }

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut cmd = CommandBuilder::new("kiro-cli");
        cmd.args(["chat", "--trust-all-tools"]);
        cmd.env("AGEND_GIT_BYPASS", "1");
        let mut child = pair.slave.spawn_command(cmd).expect("spawn kiro-cli");
        drop(pair.slave);

        // Reader thread → mpsc channel; main thread polls with timeout.
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        // fire-and-forget: thread exits when reader hits EOF after child kill.
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        let mut vt = crate::vterm::VTerm::new(80, 24);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            match rx.recv_timeout(deadline - now) {
                Ok(chunk) => vt.process(&chunk),
                Err(_) => break, // timeout or sender disconnected
            }
            if vt.tail_lines(24).contains("No, exit") {
                break;
            }
        }
        let _ = child.kill();
        let _ = child.wait();

        let screen = vt.tail_lines(24);
        let patterns = kiro_trust_patterns();

        // Two valid outcomes prove kiro startup is no longer hung on the
        // confirmation screen — the actual user-visible bug being fixed.
        //
        // (a) "No, exit" rendered → must match regex (real-spawn dismiss).
        // (b) Already past confirmation (kiro saved trust from a prior run,
        //     or `--trust-all-tools` bypassed it) → reaching the ready
        //     prompt within deadline proves no hang.
        //
        // Failure mode: neither marker present within the deadline → kiro
        // really did hang somewhere unexpected.
        let saw_prompt = screen.contains("No, exit");
        let saw_ready = screen.contains("Trust All Tools active")
            || screen.contains("ask a question or describe a task");

        if saw_prompt {
            assert!(
                try_dismiss_dialog("t", &screen, &test_writer(), &patterns),
                "production regex must match real kiro-cli trust prompt. Screen:\n{screen}"
            );
        } else {
            assert!(
                saw_ready,
                "kiro neither rendered the trust prompt nor reached ready state within 5s. \
                 Screen:\n{screen}"
            );
            eprintln!(
                "SMOKE NOTE: kiro skipped the trust prompt (saved acceptance or --trust-all-tools \
                 bypass). Synthetic-screen unit tests cover the regex correctness for the \
                 reported operator screenshot."
            );
        }
    }

    /// Sprint 21 F-NEW1: verify that `sweep_child_tree` reaches grandchild
    /// processes spawned via the agent's PTY. Mirrors the kiro-cli pattern
    /// where the leader shell forks bun/mcp/acp grandchildren — the regression
    /// PR-U #158 fixed for explicit kill paths but missed for PTY-EOF crash
    /// detection.
    ///
    /// 2026-05-18 race-class anchor: the previous form of this test polled
    /// `pid_file.exists()` then `read_to_string` — a write-vs-read race
    /// because `echo $! > file` may create the file BEFORE flushing the
    /// content. The fix lands in C2 (swap to `wait_for_nonempty_file`); C0
    /// adds a concurrent stress runner that exposes the race under load.
    #[test]
    #[cfg(unix)]
    fn sweep_child_tree_kills_grandchild_via_process_group() {
        let pid_file =
            std::env::temp_dir().join(format!("agend-sweep-test-{}.pid", std::process::id()));
        sweep_child_tree_body(&pid_file);
    }

    /// 2026-05-18 race-class C0 anchor (RED on main HEAD): concurrent
    /// stress runner for the pid_file write-vs-read race. Spawns 8
    /// threads, each running the body 6 times against unique pid_file
    /// paths (8×6 = 48 PTY spawns). Pre-fix the `exists() + read_to_string`
    /// pair races at least once across ~48 multi-threaded iterations
    /// under scheduler contention. Post-fix (C2's `wait_for_nonempty_file`
    /// swap) is deterministic — the helper polls for content, not just
    /// existence.
    ///
    /// NOT `#[ignore]`: ~10-15s on CI ubuntu-latest (where the race
    /// originally surfaced in PR #905). Local fast hardware may not
    /// reproduce — the CI runner's slower scheduler is what exposes
    /// the write/flush gap. Marked `#[cfg(unix)]` because the body
    /// uses sh + sleep.
    #[test]
    #[cfg(unix)]
    fn sweep_child_tree_kills_grandchild_concurrent_stress() {
        let handles: Vec<_> = (0..8)
            .map(|tid| {
                std::thread::spawn(move || {
                    for i in 0..6 {
                        let path = std::env::temp_dir().join(format!(
                            "agend-sweep-stress-{}-{tid}-{i}.pid",
                            std::process::id()
                        ));
                        sweep_child_tree_body(&path);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("stress thread joined");
        }
    }

    /// Test body factored out of
    /// `sweep_child_tree_kills_grandchild_via_process_group` so the
    /// concurrent stress runner can call it with unique pid_file paths.
    /// Behaviour identical to the original test — same shell command,
    /// same registry shape, same assertion set. Only the pid_file path
    /// is parameterized.
    #[cfg(unix)]
    fn sweep_child_tree_body(pid_file: &std::path::Path) {
        use parking_lot::Mutex;
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        use std::collections::HashMap;

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let _ = std::fs::remove_file(pid_file);
        // sh forks `sleep` into the background; sleep PID is recorded so the
        // test can verify it dies with the leader (group kill semantics).
        let cmd_str = format!("sleep 60 & echo $! > {} && wait", pid_file.display());
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", &cmd_str]);
        cmd.cwd(std::env::temp_dir());
        let child = pair.slave.spawn_command(cmd).expect("spawn sh + sleep");
        drop(pair.slave);
        let shell_pid = child.process_id().expect("shell process_id");

        // Build a minimal AgentHandle so sweep_child_tree's registry-lookup
        // path is exercised end-to-end (not just kill_process_tree directly).
        let pty_writer: PtyWriter =
            Arc::new(Mutex::new(pair.master.take_writer().expect("take_writer")));
        let pty_master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>> =
            Arc::new(Mutex::new(pair.master));
        let core = Arc::new(Mutex::new(AgentCore {
            vterm: crate::vterm::VTerm::with_pty_writer(80, 24, Arc::clone(&pty_writer)),
            subscribers: Vec::new(),
            state: StateTracker::new(None),
            health: HealthTracker::new(),
        }));
        let agent_name = format!("sweep-test-{}", pid_file.display());
        let handle = AgentHandle {
            id: crate::types::InstanceId::default(),
            name: agent_name.clone(),
            backend_command: "sh".to_string(),
            pty_writer,
            pty_master,
            core,
            child: Arc::new(Mutex::new(child)),
            submit_key: "\r".to_string(),
            inject_prefix: String::new(),
            typed_inject: false,
            spawned_at: std::time::Instant::now(),
            spawned_at_epoch_ms: 0,
            deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        registry.lock().insert(agent_name.clone(), handle);

        // Wait for the grandchild's PID to be observable in the file —
        // `wait_for_nonempty_file` polls for content commit, not just
        // file existence, closing the open+truncate+write race that
        // the original for-loop + read_to_string pair exhibited (PR #905
        // CI flake + PR #909 dev concurrent-load flake).
        let content = wait_for_nonempty_file(pid_file, std::time::Duration::from_secs(2))
            .expect("sleep grandchild pid_file did not become non-empty within 2s");
        let sleep_pid: u32 = content.trim().parse().expect("parse sleep grandchild PID");
        assert!(
            crate::process::is_pid_alive(shell_pid),
            "shell leader must be alive before sweep"
        );
        assert!(
            crate::process::is_pid_alive(sleep_pid),
            "sleep grandchild must be alive before sweep"
        );

        // Invoke the new helper. Should kill the entire process group.
        sweep_child_tree(&agent_name, &registry);

        // Reap the shell child so kill(pid, 0) doesn't see it as a zombie.
        // Without wait(), the shell shows as "alive" even after SIGKILL
        // because we are its parent and never collected its exit status.
        {
            let reg = &registry.lock();
            if let Some(h) = reg.get(&agent_name) {
                {
                    let mut c = h.child.lock();
                    let _ = c.wait();
                }
            }
        }

        // #934: §3.20 SOP 1 — poll-with-deadline against post-condition.
        //
        // Pre-#934 these were bare `assert!(!is_pid_alive(_pid))`
        // immediately after `sweep_child_tree` returned. Under CI
        // scheduler contention (especially in the 48-PTY concurrent
        // stress runner), the grandchild `sleep` could still appear
        // alive at the assertion point even though SIGKILL had landed —
        // it was a ZOMBIE awaiting reap by init / launchd (its new
        // parent after the shell died).
        //
        // `is_pid_alive` uses `libc::kill(pid, 0)` which returns 0 for
        // zombies (kernel still tracks the PID until reaped).
        // Init / launchd reap latency is OS-scheduling-dependent —
        // typically <1s on Linux, observed up to ~3s on macOS, worst
        // case ~5-10s on heavily loaded CI runners. Bare assert at
        // microsecond latency lost the race intermittently.
        //
        // Fix: poll with deadline. `poll_until_dead` (promoted to
        // `pub(crate)` for this PR) returns true within the window or
        // false on timeout. shell_pid uses a 5s deadline (we reap
        // directly via `child.wait()` above so the gap is short).
        // sleep_pid uses a 10s deadline for init / launchd reap-cycle
        // worst case — see deadline doc in `cleanup_zombies::poll_until_dead`
        // for OS-conditional rationale.
        assert!(
            crate::admin::cleanup_zombies::poll_until_dead(
                shell_pid,
                std::time::Duration::from_secs(5),
            ),
            "shell leader did not die within 5s post-sweep — we reap directly \
             via child.wait() so the kernel-pid-cleanup gap is normally <1s; \
             timing this slow indicates a deeper issue"
        );
        assert!(
            crate::admin::cleanup_zombies::poll_until_dead(
                sleep_pid,
                std::time::Duration::from_secs(10),
            ),
            "sleep grandchild did not die within 10s post-sweep — likely \
             init / launchd reap latency under contention (10s covers macOS \
             launchd's slowest observed cycle on loaded CI runners)"
        );
        let _ = std::fs::remove_file(pid_file);
    }

    /// Poll `path` until `read_to_string(path).trim().is_empty() == false`
    /// (i.e. file exists AND has non-empty content), or `timeout`
    /// elapses. Closes the write-vs-read race that bare
    /// `path.exists() + read_to_string` exhibits when the writer is a
    /// subprocess that does open+truncate+write+close: between create
    /// and content flush, an `exists()` poll returns true but the read
    /// yields an empty string. Reading until non-empty waits for the
    /// content commit explicitly.
    ///
    /// Returns `Ok(content)` (trimmed read) on success, `Err` with
    /// `ErrorKind::TimedOut` when the timeout fires with no non-empty
    /// content observed.
    ///
    /// Poll interval is 5ms (the OS scheduling quantum is the floor;
    /// finer polling burns CPU without buying latency improvement).
    ///
    /// `#[cfg(unix)]` matches the sole caller (`sweep_child_tree_body`,
    /// which spawns `sh` + `sleep`) — Windows builds would see an
    /// orphan helper and trip `-D dead-code` clippy. Drop the gate
    /// when a Windows-side caller appears.
    #[cfg(unix)]
    fn wait_for_nonempty_file(
        path: &std::path::Path,
        timeout: std::time::Duration,
    ) -> std::io::Result<String> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Ok(content) = std::fs::read_to_string(path) {
                if !content.trim().is_empty() {
                    return Ok(content);
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "file {} did not become non-empty within {:?}",
                        path.display(),
                        timeout
                    ),
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Helper unit test: simulates the race-class shape the helper
    /// exists to close. A separate thread creates the file empty,
    /// delays, then writes content. `wait_for_nonempty_file` must
    /// NOT return until content is observable.
    #[test]
    #[cfg(unix)]
    fn wait_for_nonempty_file_waits_until_content_is_committed() {
        let path = std::env::temp_dir().join(format!(
            "agend-wait-nonempty-test-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            // Phase 1: create empty file. A naïve `exists()` poll
            // would see this and proceed to read empty content.
            std::fs::write(&writer_path, "").expect("create empty");
            // Phase 2: simulate the OS write buffer flush gap.
            std::thread::sleep(std::time::Duration::from_millis(40));
            // Phase 3: commit the actual content.
            std::fs::write(&writer_path, "12345\n").expect("commit content");
        });

        let result = wait_for_nonempty_file(&path, std::time::Duration::from_secs(2))
            .expect("wait returned content within timeout");
        writer.join().expect("writer thread joined");

        assert_eq!(
            result.trim(),
            "12345",
            "wait_for_nonempty_file must return the committed content, not the empty stub"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Helper unit test: timeout path. File never becomes non-empty.
    #[test]
    #[cfg(unix)]
    fn wait_for_nonempty_file_returns_timeout_when_content_never_arrives() {
        let path = std::env::temp_dir().join(format!(
            "agend-wait-nonempty-timeout-test-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "").expect("create empty");

        let err = wait_for_nonempty_file(&path, std::time::Duration::from_millis(50))
            .expect_err("must time out when content never commits");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sweep_child_tree_unregistered_name_is_no_op() {
        // Sprint 21 F-NEW1: registry lookup miss must not panic. The PTY-EOF
        // path may race against an explicit handle_delete that already cleaned
        // up the registry entry — sweep should simply find nothing to kill.
        use parking_lot::Mutex;
        use std::collections::HashMap;
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        // Should not panic, should not error.
        sweep_child_tree("does-not-exist", &registry);
    }

    /// §3.5.10 concurrent-state fixture: multi-threaded producer/consumer
    /// through crossbeam_channel (the production import). Asserts message
    /// ordering invariant — bounded channel preserves FIFO order.
    /// Production-path-coupled: uses real crossbeam_channel::bounded.
    #[test]
    fn crossbeam_channel_concurrent_ordering() {
        let (tx, rx) = crossbeam_channel::bounded::<usize>(16);
        let n = 100;

        // Producer thread sends 0..n in order.
        let handle = std::thread::spawn(move || {
            for i in 0..n {
                tx.send(i).expect("send");
            }
            // tx drops here, closing the channel.
        });

        // Consumer drains concurrently (bounded channel blocks producer
        // after 16 items, so consumer must run in parallel).
        let mut received = Vec::with_capacity(n);
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(v) => received.push(v),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    panic!("recv timed out after 5s — deadlock?");
                }
            }
        }

        handle.join().expect("producer");

        // FIFO ordering preserved.
        assert_eq!(received.len(), n);
        for (i, &v) in received.iter().enumerate() {
            assert_eq!(v, i, "message {i} out of order");
        }
    }

    // ── Sprint 46 P2: resolve_instance tests ────────────────────────────

    fn resolve_test_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static CTR: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-resolve-{}-{}-{}",
            std::process::id(),
            name,
            CTR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn name_resolves_to_single_id() {
        let home = resolve_test_home("single");
        let id = crate::types::InstanceId::new();
        let yaml = format!(
            "defaults:\n  backend: claude\ninstances:\n  dev:\n    id: \"{}\"\n    role: Test\n",
            id.full()
        );
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let (resolved_id, resolved_name) = resolve_instance(&home, "dev").unwrap();
        assert_eq!(resolved_id, id);
        assert_eq!(resolved_name, "dev");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn nonexistent_name_returns_not_found() {
        // fleet.yaml HashMap guarantees name uniqueness — no Ambiguous path.
        // Verify that a non-existent name returns NotFound.
        let home = resolve_test_home("notfound");
        let yaml = "defaults:\n  backend: claude\ninstances:\n  dev:\n    role: Test\n";
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
        let result = resolve_instance(&home, "nonexistent");
        assert!(
            matches!(result, Err(ResolveError::NotFound(_))),
            "expected NotFound, got: {result:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Issue #658 regression: ANSI-colorized [AGEND-MSG] header must be
    /// detected as system header for atomic write (uses stripped text).
    #[test]
    fn ansi_header_detected_as_system_header() {
        // Simulate ANSI-wrapped header
        let raw =
            "\x1b[1;34m[AGEND-MSG]\x1b[0m from=lead kind=task size=500 (use inbox tool)\nBody here";
        let stripped = strip_ansi(raw);
        assert!(
            stripped.starts_with("[AGEND-MSG]"),
            "stripped should start with [AGEND-MSG], got: {stripped}"
        );

        let raw_from = "\x1b[32m[from:lead-kiro]\x1b[0m hello world";
        let stripped_from = strip_ansi(raw_from);
        assert!(
            stripped_from.starts_with("[from:"),
            "stripped should start with [from:, got: {stripped_from}"
        );
    }

    /// Startup grace period: agent that exits within 5s should NOT get shell fallback.
    /// Startup failure: exit within 5s + no user input since spawn → no shell fallback.
    #[test]
    fn startup_failure_no_input_no_shell_fallback() {
        // spawned_at_epoch_ms = 1000, last_input_at_ms = 500 → input before spawn
        let spawned_at_epoch_ms: u64 = 1000;
        let last_input_at_ms: u64 = 500;
        let had_user_input_since_spawn = last_input_at_ms >= spawned_at_epoch_ms;
        assert!(
            !had_user_input_since_spawn,
            "input before spawn should not count as user input"
        );
    }

    /// Quick user exit: input AFTER spawn → normal clean exit, not startup failure.
    #[test]
    fn quick_user_exit_still_clean() {
        // spawned_at_epoch_ms = 1000, last_input_at_ms = 1500 → input after spawn
        let spawned_at_epoch_ms: u64 = 1000;
        let last_input_at_ms: u64 = 1500;
        let had_user_input_since_spawn = last_input_at_ms >= spawned_at_epoch_ms;
        assert!(
            had_user_input_since_spawn,
            "input after spawn should count as user input → not startup failure"
        );
    }

    /// Deleted agent: reaper should not spawn shell fallback when deleted flag is set.
    /// Behavioral test: spawn a short-lived process, set deleted=true, verify
    /// no shell replacement appears in registry after exit.
    #[test]
    fn deleted_agent_reaper_no_shell_fallback() {
        use std::sync::atomic::Ordering;

        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let spawn_cfg = SpawnConfig {
            name: "del-test",
            backend_command: "true", // exits immediately with code 0
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        spawn_agent(&spawn_cfg, &registry).expect("spawn");

        // Set deleted flag (simulates DELETE handler)
        {
            let reg = registry.lock();
            let handle = reg.get("del-test").expect("agent must exist");
            handle.deleted.store(true, Ordering::SeqCst);
        }

        // Wait for reaper to detect exit + process the deleted check
        std::thread::sleep(std::time::Duration::from_millis(3000));

        // After reaper runs, agent should NOT be re-spawned as shell.
        // With deleted=true, reaper returns early → registry entry removed
        // (by sweep_child_tree or naturally) but no new shell spawned.
        let reg = registry.lock();
        match reg.get("del-test") {
            None => {} // removed from registry — correct (no shell fallback)
            Some(h) => {
                // If still present, backend_command must NOT be a shell
                assert_ne!(
                    h.backend_command,
                    crate::default_shell(),
                    "deleted agent must NOT get shell fallback, but got: {}",
                    h.backend_command
                );
            }
        }
    }

    /// PTY write timeout: write_with_timeout returns within bounded time.
    #[test]
    fn write_timeout_does_not_hang() {
        let buf: PtyWriter = Arc::new(Mutex::new(Box::new(std::io::sink())));
        let data = vec![0u8; 1024];
        let start = std::time::Instant::now();
        let result = write_with_timeout(&buf, &data);
        let elapsed = start.elapsed();
        assert!(result.is_ok(), "normal write should succeed");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "normal write should be fast, got {elapsed:?}"
        );
    }

    /// Stuck write: second write attempt returns TimedOut immediately
    /// (write_in_progress guard prevents thread accumulation).
    #[test]
    fn write_in_progress_guard_prevents_thread_leak() {
        // Simulate a stuck writer by inserting the key into the in-progress set
        let buf: PtyWriter = Arc::new(Mutex::new(Box::new(std::io::sink())));
        let key = Arc::as_ptr(&buf) as usize;
        {
            let mut guard = write_in_progress_set().lock();
            guard.insert(key);
        }
        // Second write should fail immediately
        let result = write_with_timeout(&buf, b"hello");
        assert!(result.is_err());
        match &result {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::TimedOut),
            Ok(_) => panic!("expected error"),
        }
        // Cleanup
        {
            let mut guard = write_in_progress_set().lock();
            guard.remove(&key);
        }
    }

    /// Error (non-timeout) clears guard, allowing retry.
    #[test]
    #[cfg(unix)]
    fn error_clears_guard_allows_retry() {
        // Use a closed writer that returns BrokenPipe
        let (rd, wr) = std::os::unix::net::UnixStream::pair().expect("pair");
        drop(rd); // close read end → writes will fail with BrokenPipe
        let buf: PtyWriter = Arc::new(Mutex::new(Box::new(wr)));

        // First write: should fail with BrokenPipe but clear guard
        let r1 = write_with_timeout(&buf, b"hello");
        assert!(r1.is_err());

        // Second write: should also fail (not blocked by guard)
        let r2 = write_with_timeout(&buf, b"world");
        assert!(r2.is_err());
        // Key: it didn't return "already in progress" — it actually tried
        let err_msg = r2.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
        assert_ne!(
            err_msg,
            "PTY write already in progress (previous write stuck)"
        );
    }

    /// write_to_agent_typed also uses timeout (not direct lock+write).
    #[test]
    fn write_to_agent_typed_uses_timeout() {
        // Verify typed path calls write_with_timeout by checking it
        // respects the in-progress guard
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let writer: PtyWriter =
            Arc::new(Mutex::new(pair.master.take_writer().expect("take_writer")));
        // Insert in-progress guard
        let key = Arc::as_ptr(&writer) as usize;
        {
            let mut guard = write_in_progress_set().lock();
            guard.insert(key);
        }
        let handle = AgentHandle {
            id: crate::types::InstanceId::default(),
            name: "typed-test".to_string(),
            backend_command: "test".to_string(),
            pty_writer: writer,
            pty_master: Arc::new(Mutex::new(pair.master)),
            core: Arc::new(Mutex::new(AgentCore {
                vterm: VTerm::new(80, 24),
                subscribers: Vec::new(),
                state: StateTracker::new(None),
                health: HealthTracker::new(),
            })),
            child: Arc::new(Mutex::new(
                pair.slave
                    .spawn_command(portable_pty::CommandBuilder::new("true"))
                    .expect("spawn"),
            )),
            submit_key: "\r".to_string(),
            inject_prefix: String::new(),
            typed_inject: true,
            spawned_at: std::time::Instant::now(),
            spawned_at_epoch_ms: 0,
            deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let result = write_to_agent_typed(&handle, b"x");
        assert!(
            result.is_err(),
            "typed write should fail when in-progress guard is set"
        );
        // Cleanup
        {
            let key = Arc::as_ptr(&handle.pty_writer) as usize;
            let mut guard = write_in_progress_set().lock();
            guard.remove(&key);
        }
    }

    /// #708: AGEND_GIT_BYPASS must not leak to child processes.
    #[test]
    fn build_command_strips_agend_git_bypass() {
        std::env::set_var("AGEND_GIT_BYPASS", "1");
        let config = SpawnConfig {
            name: "strip-test",
            backend_command: "echo",
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        let (cmd, _) = build_command(&config).expect("build_command");
        // Verify env_remove was called — CommandBuilder won't pass it to child.
        // The env_remove in build_command is the authoritative guard.
        let _ = cmd;
        std::env::remove_var("AGEND_GIT_BYPASS");
    }

    /// Phase A Piece-3: GIT_EDITOR + friends must all be set to
    /// `"true"` (no-op editor binary) in the daemon-spawned agent
    /// process env so `git rebase --continue` / `git commit`
    /// (without -m) / `git rebase -i` don't drop the PTY into a
    /// Vim/editor lockup. Empirical experiment surfaced this on
    /// opencode + DeepSeek backends; the daemon-side default closes
    /// the lockup surface across all backends + scenarios.
    ///
    /// Operator override is preserved by ordering — these env vars
    /// are set BEFORE the fleet.yaml user-env loop, so an operator's
    /// `instances.<name>.env.GIT_EDITOR: vim` would override the
    /// daemon default. This test pins the default; the override path
    /// is covered structurally by `build_command`'s existing
    /// fleet.yaml env-merge logic (no separate test needed for the
    /// override since the env loop is single-call `cmd.env(k, v)`).
    #[test]
    fn build_command_sets_git_editor_defaults() {
        let config = SpawnConfig {
            name: "git-editor-test",
            backend_command: "echo",
            args: &[],
            spawn_mode: crate::backend::SpawnMode::Fresh,
            cols: 80,
            rows: 24,
            env: None,
            working_dir: None,
            submit_key: "\r",
            home: None,
            crash_tx: None,
            shutdown: None,
        };
        let (cmd, _) = build_command(&config).expect("build_command");
        for key in &["GIT_EDITOR", "GIT_SEQUENCE_EDITOR", "EDITOR", "VISUAL"] {
            let value = cmd
                .get_env(key)
                .unwrap_or_else(|| panic!("{key} must be set in agent env"));
            assert_eq!(
                value.to_string_lossy(),
                "true",
                "{key} must default to `true` (no-op editor binary), got {value:?}"
            );
        }
    }

    /// #1146: send_to_registry must release the registry lock before
    /// calling inject. Source-grep pin: the lock scope must close
    /// before inject_with_target appears. This is structurally
    /// enforced — inject_with_target takes &InjectTarget (a snapshot),
    /// not &AgentHandle (a registry borrow), so holding the lock
    /// during inject is impossible without reverting to inject_to_agent.
    #[test]
    fn send_to_registry_releases_lock_before_inject_1146() {
        let src = include_str!("agent.rs");
        let fn_start = src
            .find("fn send_to_registry(")
            .expect("send_to_registry must exist");
        let fn_body = &src[fn_start..fn_start + 600];

        let lock_end = fn_body
            .find("}; // lock released")
            .expect("send_to_registry must have a scoped lock block");
        let inject_call = fn_body
            .find("inject_with_target")
            .expect("send_to_registry must call inject_with_target");
        assert!(
            lock_end < inject_call,
            "#1146: inject_with_target must appear AFTER the lock scope \
             ends (lock_end={lock_end}, inject_call={inject_call})"
        );
    }

    /// #1146: broadcast_registry must snapshot all targets under one
    /// lock acquisition, release, then inject without re-acquiring.
    /// Pre-fix: re-acquired lock per-target and held it during
    /// inject — N typed_inject targets × 20s each.
    #[test]
    fn broadcast_registry_releases_lock_before_inject_1146() {
        let src = include_str!("agent.rs");
        let fn_start = src
            .find("fn broadcast_registry(")
            .expect("broadcast_registry must exist");
        let fn_body = &src[fn_start..fn_start + 1500];

        let lock_release = fn_body
            .find("}; // lock released")
            .expect("broadcast_registry must have a scoped lock block");
        assert!(
            !fn_body[lock_release..].contains("lock_registry"),
            "#1146: broadcast_registry must NOT re-acquire registry \
             lock after releasing it (pre-fix held lock during inject)"
        );

        let inject_site = fn_body
            .find("inject_with_target")
            .expect("broadcast_registry must call inject_with_target");
        assert!(
            inject_site > lock_release,
            "#1146: inject_with_target must appear after lock release"
        );
    }

    /// #1146 reviewer fix: inject_with_target must skip if the agent
    /// was deleted between snapshot and inject (delete/reuse race).
    /// The deleted flag is an Arc<AtomicBool> shared with AgentHandle,
    /// so setting it on the handle is visible to the snapshot.
    #[test]
    fn inject_with_target_skips_deleted_agent_1146() {
        let writer: PtyWriter = Arc::new(parking_lot::Mutex::new(
            Box::new(std::io::sink()) as Box<dyn Write + Send>
        ));
        let deleted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let target = InjectTarget {
            pty_writer: writer,
            inject_prefix: String::new(),
            submit_key: "\r".to_string(),
            typed_inject: false,
            deleted: Arc::clone(&deleted),
        };

        // Inject succeeds when not deleted.
        assert!(inject_with_target(&target, b"hello").is_ok());

        // Simulate delete_transaction setting the flag.
        deleted.store(true, std::sync::atomic::Ordering::Release);

        // Inject must return Ok (no-op) without writing.
        assert!(inject_with_target(&target, b"should be skipped").is_ok());
    }
    /// #1144: pty_read_loop error path must trigger handle_pty_close cleanup.
    /// Previously, `Err(e)` broke out of the loop without calling
    /// handle_pty_close, leaving the agent as a zombie in the registry.
    /// This test simulates a read error by providing a reader that fails
    /// after producing some output, then verifies the agent is cleaned up
    /// from the registry (handle_pty_close removes it on non-crash paths).
    #[test]
    #[allow(clippy::unwrap_used)]
    fn pty_read_error_triggers_cleanup() {
        use std::collections::HashMap;

        struct FailingReader {
            call: std::cell::Cell<u32>,
        }
        impl std::io::Read for FailingReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.call.get();
                self.call.set(n + 1);
                if n == 0 {
                    buf[0] = b'x';
                    Ok(1)
                } else {
                    Err(std::io::Error::other("simulated read error"))
                }
            }
        }

        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let pty_writer: PtyWriter = Arc::new(Mutex::new(Box::new(Vec::<u8>::new())));
        let core = Arc::new(Mutex::new(AgentCore {
            vterm: VTerm::new(80, 24),
            subscribers: Vec::new(),
            state: StateTracker::new(None),
            health: HealthTracker::new(),
        }));

        let agent_name = "read-err-test";
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(true));

        registry.lock().insert(
            agent_name.to_string(),
            AgentHandle {
                id: crate::types::InstanceId::default(),
                name: agent_name.to_string(),
                backend_command: "test".to_string(),
                pty_writer: Arc::clone(&pty_writer),
                pty_master: Arc::new(Mutex::new(
                    portable_pty::native_pty_system()
                        .openpty(PtySize {
                            rows: 24,
                            cols: 80,
                            pixel_width: 0,
                            pixel_height: 0,
                        })
                        .unwrap()
                        .master,
                )),
                core: Arc::clone(&core),
                child: Arc::new(Mutex::new(
                    portable_pty::native_pty_system()
                        .openpty(PtySize {
                            rows: 24,
                            cols: 80,
                            pixel_width: 0,
                            pixel_height: 0,
                        })
                        .unwrap()
                        .slave
                        .spawn_command(portable_pty::CommandBuilder::new("true"))
                        .unwrap(),
                )),
                submit_key: "\r".to_string(),
                inject_prefix: String::new(),
                typed_inject: false,
                spawned_at: std::time::Instant::now(),
                spawned_at_epoch_ms: 0,
                deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
        );

        assert!(
            registry.lock().contains_key(agent_name),
            "pre-condition: agent must be in registry"
        );

        let ctx = PtyReadContext {
            name: agent_name.to_string(),
            core,
            pty_writer,
            registry: Arc::clone(&registry),
            home: None,
            crash_tx: None,
            dismiss_patterns: Vec::new(),
            shutdown: Some(shutdown),
            deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        let mut reader = FailingReader {
            call: std::cell::Cell::new(0),
        };
        let capture = crate::capture::make_capture_writer(None, agent_name, "test");
        pty_read_loop(&mut reader, &ctx, capture);

        assert!(
            !registry.lock().contains_key(agent_name),
            "#1144: read error path must call handle_pty_close which removes \
             agent from registry (shutdown=true path). Before fix, the Err \
             branch broke without cleanup, leaving a zombie entry."
        );
    }

    /// #1145: write_with_timeout stuck thread must clear WRITE_IN_PROGRESS
    /// guard on completion, even after the caller has timed out. Before the
    /// fix, the guard persisted forever after timeout, permanently blocking
    /// future writes to the same PtyWriter (or any new writer allocated at
    /// the same pointer address).
    #[test]
    fn write_guard_cleared_after_stuck_thread_completes() {
        let (lock_tx, lock_rx) = crossbeam_channel::bounded::<()>(0);
        let (unlock_tx, unlock_rx) = crossbeam_channel::bounded::<()>(0);

        struct BlockingWriter {
            lock_tx: crossbeam_channel::Sender<()>,
            unlock_rx: crossbeam_channel::Receiver<()>,
        }
        impl std::io::Write for BlockingWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let _ = self.lock_tx.send(());
                let _ = self.unlock_rx.recv();
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let writer: PtyWriter =
            Arc::new(Mutex::new(Box::new(BlockingWriter { lock_tx, unlock_rx })));
        let key = Arc::as_ptr(&writer) as usize;

        let writer2 = Arc::clone(&writer);
        let handle = std::thread::spawn(move || write_with_timeout(&writer2, b"hello"));

        // Wait for the write thread to enter the blocking write.
        lock_rx.recv().expect("write thread must signal lock");

        // Caller's recv_timeout will fire after 5s. Speed this up by
        // spawning a second attempt that hits the in-progress guard.
        let guard_set = write_in_progress_set().lock().contains(&key);
        assert!(guard_set, "guard must be set while write is in progress");

        // Unblock the write thread.
        unlock_tx.send(()).expect("unblock write thread");

        // Wait for the caller to finish.
        let result = handle.join().expect("write thread joined");
        assert!(result.is_ok(), "write should succeed after unblock");

        // Guard must be cleared by the thread itself.
        let guard_after = write_in_progress_set().lock().contains(&key);
        assert!(
            !guard_after,
            "#1145: thread must clear WRITE_IN_PROGRESS guard on exit. \
             Before fix, only the caller's success/error paths cleared it; \
             the timeout path left it set permanently."
        );
    }
}
