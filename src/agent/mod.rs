//! Agent state and PTY management.
//!
//! Sync design: std::thread for PTY I/O, crossbeam broadcast for output distribution.
//! Single Mutex on AgentCore ensures atomic subscribe+dump.

use crate::backend::Backend;
use crate::health::HealthTracker;
use crate::state::StateTracker;
use crate::sync_audit::CoreMutex;
use crate::vterm::VTerm;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

mod dismiss;
#[allow(unused_imports)]
pub use dismiss::try_dismiss_dialog;
use dismiss::{prepare_dismiss_patterns, try_prepared_dismiss_dialog, PreparedDismissPattern};

pub mod deleting;

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
    pub(crate) core: Arc<CoreMutex<AgentCore>>,
    pub(crate) child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    pub(crate) submit_key: String,
    pub(crate) inject_prefix: String,
    pub(crate) typed_inject: bool,
    pub(crate) spawned_at: std::time::Instant,
    pub(crate) spawned_at_epoch_ms: u64,
    /// Set by DELETE handler to prevent reaper from spawning shell fallback.
    pub(crate) deleted: Arc<std::sync::atomic::AtomicBool>,
}

// #1441: keyed by stable InstanceId (UUID), NOT name. Live-process identity
// (PTY inject / pane subscription) now shares the same UUID authority as inbox
// delivery (both via crate::fleet::resolve_uuid). Name→id resolution always
// goes through that single resolver; the registry never holds its own name index.
pub type AgentRegistry = Arc<Mutex<HashMap<crate::types::InstanceId, AgentHandle>>>;

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
];

/// Returns true if the env-var name is on the spawn-time deny-list.
pub fn is_sensitive_env_key(key: &str) -> bool {
    SENSITIVE_ENV_KEYS
        .iter()
        .any(|denied| denied.eq_ignore_ascii_case(key))
}

/// #1440: base runtime env allowlist — the minimum any agent CLI needs to
/// launch and reach its provider. Injected only if present in the daemon env
/// (so Windows-only keys are harmless on Unix and vice versa). Corp-specific
/// extras (`NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, …) are intentionally absent
/// — operators name those in `passthrough_env`, keeping the default minimal
/// and auditable.
const BASE_ENV_ALLOWLIST: &[&str] = &[
    // Identity / shell / paths
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    // Locale
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "TZ",
    // Temp dirs
    "TMPDIR",
    "TMP",
    "TEMP",
    // Agent IO / auth socket
    "SSH_AUTH_SOCK",
    // XDG base dirs
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    // Proxies (lower + upper case both seen in the wild)
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    // Windows essentials — inject-if-present; absent on Unix. Without these a
    // child process fails to start on Windows (CI runs windows-latest).
    "SYSTEMROOT",
    "SystemDrive",
    "windir",
    "PATHEXT",
    "COMSPEC",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// #1440: is agent-backend env isolation enabled? Default OFF (phased rollout
/// — this version does not change default spawn behavior).
pub fn env_isolation_enabled() -> bool {
    std::env::var("AGEND_ENV_ISOLATION").as_deref() == Ok("1")
}

/// #1440: outcome of [`resolve_child_env`].
pub struct ChildEnvPlan {
    /// `(key, value)` pairs to inject into the child after `env_clear()`.
    pub injected: Vec<(String, String)>,
    /// Names of source-env vars that would NOT survive isolation (warn input).
    pub dropped: Vec<String>,
}

/// Case-insensitive membership (matches `is_sensitive_env_key`; also handles
/// Windows' case-insensitive env-var names, e.g. `Path` vs `PATH`).
fn env_key_in(key: &str, list: &[&str]) -> bool {
    list.iter().any(|k| k.eq_ignore_ascii_case(key))
}

/// #1440: PURE — decide which inherited env vars survive isolation. A var
/// survives iff it is in the base allowlist, OR a credential key the detected
/// backend declares (these intentionally override `SENSITIVE_ENV_KEYS` for the
/// owning backend only → cross-backend credential isolation), OR an operator
/// `passthrough` key that is NOT itself sensitive (so `LD_PRELOAD` stays
/// blocked even if listed). `source_env` is a snapshot — the real daemon env
/// in production, an injected map in tests.
pub fn resolve_child_env(
    backend: Option<&Backend>,
    passthrough: &[String],
    source_env: &std::collections::BTreeMap<String, String>,
) -> ChildEnvPlan {
    let creds: &[&str] = backend.map(|b| b.credential_env_keys()).unwrap_or(&[]);
    let pass: Vec<&str> = passthrough.iter().map(String::as_str).collect();
    let mut injected = Vec::new();
    let mut dropped = Vec::new();
    for (k, v) in source_env {
        let allowed = env_key_in(k, BASE_ENV_ALLOWLIST)
            || env_key_in(k, creds)
            || (env_key_in(k, &pass) && !is_sensitive_env_key(k));
        if allowed {
            injected.push((k.clone(), v.clone()));
        } else {
            dropped.push(k.clone());
        }
    }
    injected.sort();
    dropped.sort();
    ChildEnvPlan { injected, dropped }
}

/// #1440: one-time warning listing inherited env var KEY NAMES (never values)
/// that would be dropped under isolation. Lets operators preview the impact
/// before flipping `AGEND_ENV_ISOLATION=1`.
fn warn_env_isolation_disabled_once(dropped: &[String]) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        if dropped.is_empty() {
            return;
        }
        tracing::warn!(
            dropped_keys = %dropped.join(", "),
            "AGEND_ENV_ISOLATION off: agent backends inherit the full daemon env. \
             Under isolation these inherited keys would be dropped (names only). \
             Opt in with AGEND_ENV_ISOLATION=1 + passthrough_env."
        );
    });
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
            if inst.id.as_deref().and_then(crate::types::InstanceId::parse) == Some(id) {
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
        // CR-2026-06-14: an instance whose fleet.yaml `id` is absent or
        // unparseable has NO stable identity. The prior `.unwrap_or_default()`
        // minted a FRESH random UUID on every call (`InstanceId::default()` =
        // `new_v4`) — non-deterministic, silently breaking every id-keyed
        // correlation (routing / task-event emitter / dedup / audit). Refuse
        // instead of fabricating, matching the authoritative
        // `fleet::resolve_uuid` (which returns `None` for the same case).
        // Callers degrade correctly: task_events → no emitter id; comms → falls
        // through to name/team routing.
        return match inst.id.as_deref().and_then(crate::types::InstanceId::parse) {
            Some(id) => Ok((id, name_or_id.to_string())),
            None => Err(ResolveError::NotFound(name_or_id.to_string())),
        };
    }

    Err(ResolveError::NotFound(name_or_id.to_string()))
}

/// Lock the agent registry, recovering from poison.
pub fn lock_registry(reg: &AgentRegistry) -> RegistryGuard<'_> {
    crate::sync_audit::assert_lock_tier(1, "registry");
    let inner = reg.lock();
    // #1492: mark the registry lock held so self-IPC vectors can detect a
    // lock-across-IPC deadlock. Debug-only; release no-op.
    crate::sync_audit::registry_lock_entered();
    RegistryGuard { inner }
}

/// #2050 simplify: the live-agent-name registry-snapshot idiom — a `HashSet`
/// of the current handle names taken under the registry lock — centralized
/// from the 4 daemon-lane copies (`supervisor` + `supervisor_trackers`). The 3
/// app-lane sites build the set inline before a `.filter` and are left as-is
/// (outside this lane).
pub(crate) fn live_agent_names(registry: &AgentRegistry) -> std::collections::HashSet<String> {
    lock_registry(registry)
        .values()
        .map(|h| h.name.to_string())
        .collect()
}

/// `Vec` sibling of [`live_agent_names`] for the one caller
/// (`check_pane_input_not_submitted`) that needs an ordered `&[String]` slice.
pub(crate) fn live_agent_names_vec(registry: &AgentRegistry) -> Vec<String> {
    lock_registry(registry)
        .values()
        .map(|h| h.name.to_string())
        .collect()
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
    // #1492: see lock_registry. Debug-only; release no-op.
    crate::sync_audit::registry_lock_entered();
    RegistryGuard { inner }
}

/// RAII guard returned by [`lock_registry_tracked`]. Deref's to the
/// underlying `HashMap<String, AgentHandle>`. On drop, clears
/// `REGISTRY_HOLDER` then the inner `MutexGuard` releases the lock
/// (field drop order). The brief slot-cleared-before-lock-released
/// window is harmless — the next acquirer's own `set_registry_holder`
/// fires immediately after their acquire.
pub struct RegistryGuard<'a> {
    inner: parking_lot::MutexGuard<
        'a,
        std::collections::HashMap<crate::types::InstanceId, AgentHandle>,
    >,
}

impl<'a> std::ops::Deref for RegistryGuard<'a> {
    type Target = std::collections::HashMap<crate::types::InstanceId, AgentHandle>;
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
        // #1492: clear the held-flag as the registry scope ends (debug-only;
        // release no-op). The inner MutexGuard releases via field-drop right
        // after this body.
        crate::sync_audit::registry_lock_exited();
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

/// #1504 L1: split `path` with the platform PATH separator (`;` on Windows,
/// `:` elsewhere) and drop the shim dir (`$AGEND_HOME/bin`). Pure so the
/// exclusion is unit-testable without a live env. `split_paths` correctly
/// handles Windows drive-colons and quoted entries — the bug the old
/// `.split(':')` introduced.
fn git_search_without_shim(
    path: &std::ffi::OsStr,
    shim_dir: Option<&std::path::Path>,
) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|p| !p.as_os_str().is_empty())
        .filter(|p| !same_dir(p, shim_dir))
        .collect()
}

/// True when `a` and `b` name the same directory. Prefers `canonicalize`
/// (resolves slash form + case-folds on Windows NTFS + follows symlinks);
/// falls back to a lexical compare when either side doesn't exist on disk
/// (e.g. `$AGEND_HOME/bin` not yet created — a common case). NEVER unwraps
/// `canonicalize`: it `Err`s on missing paths (#1504).
fn same_dir(a: &std::path::Path, b: Option<&std::path::Path>) -> bool {
    let Some(b) = b else { return false };
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => lexical_path_eq(a, b),
    }
}

/// Lexical directory equality fallback: normalize backslashes to forward
/// slashes, strip trailing separators, and compare case-insensitively on
/// Windows. Used only when a path can't be canonicalized (doesn't exist yet).
fn lexical_path_eq(a: &std::path::Path, b: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| {
        p.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string()
    };
    let (na, nb) = (norm(a), norm(b));
    if cfg!(windows) {
        na.eq_ignore_ascii_case(&nb)
    } else {
        na == nb
    }
}

/// Build a `CommandBuilder` with resolved args, env, and working directory.
///
/// #1519: per-instance opencode data dir (used as `XDG_DATA_HOME`). opencode
/// writes its session DB + auth under `$XDG_DATA_HOME/opencode/`; keying this
/// on the instance name gives each opencode agent an isolated session DB.
/// Pure — testable without a real spawn. Lives under `$AGEND_HOME/backend-data`
/// so the instance-delete teardown can GC it (see `full_delete_instance`).
pub(crate) fn opencode_data_dir(home: &std::path::Path, instance: &str) -> PathBuf {
    home.join("backend-data").join("opencode").join(instance)
}

/// #1519: the per-instance `XDG_DATA_HOME` to inject for an opencode spawn, or
/// `None` for any other backend / when `home` is unknown. Pure gate so the
/// OpenCode-only behavior (and the two-distinct-instances invariant) is
/// unit-testable without inspecting a `CommandBuilder`.
fn per_instance_opencode_xdg(
    backend: Option<&Backend>,
    home: Option<&std::path::Path>,
    instance: &str,
) -> Option<PathBuf> {
    match (backend, home) {
        (Some(Backend::OpenCode), Some(h)) => Some(opencode_data_dir(h, instance)),
        _ => None,
    }
}

/// #1519: canonical opencode `auth.json` to seed each per-instance data dir
/// from. opencode uses XDG semantics on every platform (`~/.local/share`, NOT
/// macOS's `~/Library/Application Support`), so resolve it the XDG way from the
/// daemon's CURRENT env — which still holds the operator's (pre-override)
/// `XDG_DATA_HOME` at spawn time. `None` if neither `XDG_DATA_HOME` nor `HOME`
/// is set.
fn canonical_opencode_auth() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
        })?;
    Some(base.join("opencode").join("auth.json"))
}

/// #1519: prepare a per-instance opencode data dir: create `<xdg>/opencode/`
/// and copy the canonical credential in (mode 600) so the isolated DB dir is
/// still authenticated. `auth_src` is passed in (not read from env) so this is
/// deterministically testable; a missing/absent source is a no-op (the agent
/// then surfaces opencode's own "not logged in" prompt rather than silently
/// sharing the global session).
fn provision_opencode_data_dir(
    xdg_dir: &std::path::Path,
    auth_src: Option<&std::path::Path>,
) -> std::io::Result<()> {
    let oc_dir = xdg_dir.join("opencode");
    std::fs::create_dir_all(&oc_dir)?;
    if let Some(src) = auth_src {
        if src.exists() {
            let dst = oc_dir.join("auth.json");
            std::fs::copy(src, &dst)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    Ok(())
}

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

    // #1440: agent-backend env isolation. INVARIANT: env_clear() must run here,
    // before any env injection below, or the explicit keys we set would be wiped.
    // Default off (phased rollout) — when off, behavior is unchanged except a
    // one-time warn listing the inherited keys that isolation would drop.
    {
        let source_env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
        let passthrough = (*home)
            .and_then(|h| crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(h)).ok())
            .map(|f| f.resolve_passthrough_env(name))
            .unwrap_or_default();
        let plan = resolve_child_env(detected_backend.as_ref(), &passthrough, &source_env);
        if env_isolation_enabled() {
            cmd.env_clear();
            for (k, v) in &plan.injected {
                cmd.env(k, v);
            }
        } else {
            warn_env_isolation_disabled_once(&plan.dropped);
        }
    }

    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("FORCE_COLOR", "1");
    cmd.env("AGEND_INSTANCE_NAME", name);
    // MED-5: AGEND_HOME is on the env-isolation SENSITIVE deny-list (so
    // `env_clear` drops it and the fleet passthrough loop never re-adds it), but
    // the spawned agent's in-pane `agend-terminal` subcommands need it to resolve
    // the daemon's home — without it they fall back to the default
    // `~/.agend-terminal`, pointing at the wrong daemon. Re-inject unconditionally
    // after the clear, exactly like AGEND_INSTANCE_NAME above.
    if let Some(h) = *home {
        cmd.env("AGEND_HOME", h);
    }

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
    //
    // #2106: EXCEPTION — a sensitive key is allowed through when it is a
    // credential the detected backend declares (`credential_env_keys`, the same
    // per-backend override the #1440 isolation path applies in
    // `resolve_child_env`). Operators legitimately point an instance at a
    // third-party provider (e.g. an `ANTHROPIC_BASE_URL` proxy +
    // `ANTHROPIC_AUTH_TOKEN`), and the operator's fleet.yaml is a TRUSTED source
    // under the single-machine single-user threat model (anyone who can write
    // fleet.yaml can already run arbitrary code as the user). This stays NARROW:
    // only THIS backend's own credential keys pass — linker-injection
    // (`LD_*`/`DYLD_*`), home/identity redirect (`AGEND_HOME`,
    // `AGEND_INSTANCE_NAME`), and foreign credentials (`AWS_*`, `GITHUB_TOKEN`,
    // another backend's key) are NOT in this backend's credential set, so they
    // are still dropped even from fleet.yaml. `is_sensitive_env_key` / the
    // deny-list itself are unchanged, so every OTHER enforcement point (the
    // isolation passthrough, etc.) keeps its protection.
    let backend_creds: &[&str] = detected_backend
        .as_ref()
        .map(|b| b.credential_env_keys())
        .unwrap_or(&[]);
    if let Some(env_map) = *env {
        for (k, v) in env_map {
            if is_sensitive_env_key(k) {
                if env_key_in(k, backend_creds) {
                    // Audit the override (key NAME only — never the secret value).
                    // One line per spawn of an instance that sets a per-instance
                    // credential; not a per-tick flood.
                    tracing::info!(
                        instance = %name,
                        key = %k,
                        "#2106: injecting operator fleet.yaml credential override (backend-declared credential key)"
                    );
                } else {
                    tracing::warn!(
                        instance = %name,
                        key = %k,
                        "dropping fleet.yaml env override for sensitive key"
                    );
                    continue;
                }
            }
            cmd.env(k, v);
        }
    }

    // #1519: per-instance opencode session isolation. opencode stores ALL
    // sessions in a single global DB under XDG_DATA_HOME (default
    // ~/.local/share/opencode/opencode.db), and `--continue` resumes the
    // GLOBAL most-recent session regardless of cwd — so two opencode instances
    // (fixup-reviewer + fixup-reviewer-2) shared one session byte-for-byte.
    // Give each instance its OWN XDG_DATA_HOME → its own DB → its own
    // `--continue` history (ResumeMode unchanged). opencode also reads its
    // credential from `$XDG_DATA_HOME/opencode/auth.json`, so copy the canonical
    // auth in (it's a static api key; copy is robust against opencode's
    // atomic-rename writes that would sever a symlink — and reviewer-2 confirmed
    // OPENCODE_API_KEY env isn't honored). Gated to OpenCode only.
    //
    // Placement is DELIBERATE: this runs AFTER the fleet.yaml user-env loop and
    // the #1440 allowlist block so the per-instance value overrides BOTH an
    // on-allowlist XDG_DATA_HOME and an operator-set one — session isolation is
    // a correctness invariant that must win over operator preference here.
    if let Some(data_dir) = per_instance_opencode_xdg(detected_backend.as_ref(), *home, name) {
        if let Err(e) = provision_opencode_data_dir(&data_dir, canonical_opencode_auth().as_deref())
        {
            tracing::warn!(
                instance = %name,
                error = %e,
                "#1519: opencode data-dir provision failed; instance may fall back to the shared session"
            );
        }
        cmd.env("XDG_DATA_HOME", &data_dir);
    }

    // #1956: disable opencode's interactive self-update prompt. opencode pops a
    // "A newer release is available. Would you like to update now?" modal on
    // startup/idle when a newer release exists — it hangs the ENTIRE pane (the
    // agent can't receive or answer any dispatch; an ESC mis-fires Confirm and
    // self-updates mid-session). There is no `--no-update` CLI flag, but opencode
    // MERGES `OPENCODE_CONFIG_CONTENT` on top of the user's global
    // `~/.config/opencode/opencode.json` (verified MERGE, not replace, via
    // `opencode debug config` — the user's provider/auth config is preserved),
    // and its config schema has an `autoupdate` field. Inject it inline so
    // nothing lands on disk and the user's global config is never touched. Gated
    // to OpenCode (same self-modification-footgun discipline as the XDG block).
    if matches!(detected_backend, Some(Backend::OpenCode)) {
        cmd.env("OPENCODE_CONFIG_CONTENT", r#"{"autoupdate":false}"#);
        // #1970: the config injection above covers FRESH spawns but NOT the
        // daemon-restart respawn (`opencode --continue`): on the resume path
        // the update-check's `getGlobal()` config does not reflect
        // OPENCODE_CONFIG_CONTENT, so the modal came back on every daemon
        // restart. Binary-verified (opencode 1.15.13): the check is a single
        // function gated `if (config.autoupdate === false ||
        // env.OPENCODE_DISABLE_AUTOUPDATE) return;`, the env side reads a
        // process.env snapshot taken at startup (independent of config/session
        // loading), and the update modal is driven exclusively by the
        // `installation.update-available` event emitted AFTER that gate — so
        // the env covers resume where the config side could not. Keep BOTH:
        // config as the documented belt, env as the resume-proof suspenders.
        cmd.env("OPENCODE_DISABLE_AUTOUPDATE", "1");
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
        // #1504: build the git search PATH using the platform-aware separator.
        // The prior `.split(':')` was hardcoded Unix; on Windows PATH is
        // `;`-separated AND entries carry drive-colons (`C:\…`), so `:`-splitting
        // shredded PATH → `which_in` failed → AGEND_REAL_GIT never got injected →
        // the shim later resolved git to itself (recursive-spawn storm, #1504 L1).
        let agend_bin: Option<PathBuf> = home.map(|h| h.join("bin"));
        let path_os = std::env::var_os("PATH").unwrap_or_default();
        let search = std::env::join_paths(git_search_without_shim(&path_os, agend_bin.as_deref()))
            .unwrap_or_default();
        if let Ok(git_path) = which::which_in("git", Some(search), ".") {
            cmd.env("AGEND_REAL_GIT", git_path);
        }
    }

    // #1597: the resolved cwd we actually hand the PTY — captured so the agy
    // `$PWD` arm below decides on (and points at) the SAME path agy will see.
    let mut spawn_cwd: Option<PathBuf> = None;
    if let Some(dir) = working_dir {
        // Defense-in-depth: the API spawn handler already calls
        // validate_working_directory at admission, but a symlink could have
        // been swapped in between admission and spawn. Revalidate here both
        // before and after create_dir_all so the final cwd we hand to the PTY
        // provably resolves inside AGEND_HOME / AGEND_ALLOWED_ROOTS.
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
            spawn_cwd = Some(resolved);
        } else {
            tracing::warn!(
                instance = %name,
                dir = %dir.display(),
                "spawn without AGEND_HOME — working_directory symlink recheck skipped"
            );
            std::fs::create_dir_all(dir).ok();
            cmd.cwd(dir);
            spawn_cwd = Some(dir.to_path_buf());
        }
    }

    // #1547 (A): agy rejects a workspace whose path has a dot-prefixed (hidden)
    // ancestor and reads the workspace path from `$PWD` (not getcwd/realpath;
    // operator e2e-verified). #1597 makes this CONDITIONAL on the resolved cwd:
    //
    // - cwd has NO hidden component (e.g. an explicit non-hidden
    //   `working_directory`) → agy accepts it directly. Set `$PWD` to the real
    //   cwd and SKIP the link, so agy reports the user's actual dir and we leave
    //   no stray symlink shadowing it.
    // - cwd has a hidden ancestor (the default `$AGEND_HOME/workspace/<name>`
    //   under `~/.agend-terminal`) → agy would reject it. Point `$PWD` at a
    //   NON-hidden link to the same dir (the cwd stays the real allowed root).
    //   This is the case #1547/#1582 exists for.
    //
    // Done LAST — after the fleet.yaml user-env loop — so the daemon value is
    // authoritative. Link failure is non-fatal (agy still spawns, just without
    // fleet MCP; the boot invariant in `spawn_agent` warns).
    if matches!(detected_backend, Some(Backend::Agy)) {
        if let (Some(cwd), Some(home_path)) = (spawn_cwd.as_deref(), home) {
            if crate::agy_workspace::path_has_hidden_component(cwd) {
                match crate::agy_workspace::ensure_link(home_path, name, cwd) {
                    Ok(link) => {
                        cmd.env("PWD", &link);
                        tracing::debug!(
                            instance = %name, pwd = %link.display(),
                            "agy: hidden workspace — $PWD set to non-hidden link"
                        );
                    }
                    Err(e) => tracing::warn!(
                        instance = %name, error = %e,
                        "agy: could not create non-hidden workspace link — agy will \
                         reject the hidden workspace and load no fleet MCP"
                    ),
                }
            } else {
                // Non-hidden cwd: agy accepts it as-is — no link needed.
                cmd.env("PWD", cwd);
                tracing::debug!(
                    instance = %name, pwd = %cwd.display(),
                    "agy: non-hidden workspace — $PWD set to the real dir (no link)"
                );
            }
        }
    }

    // #708: strip AGEND_GIT_BYPASS from child env — agents must use the
    // git shim (which checks the var), not inherit a blanket bypass.
    cmd.env_remove("AGEND_GIT_BYPASS");

    Ok((cmd, detected_backend))
}

/// Resolve the authoritative registry `InstanceId` for a spawn (#1441).
///
/// A *managed* spawn (`home = Some`: a fleet/inbox-backed agent) MUST resolve to
/// its fleet.yaml UUID — a missing entry would force a random fallback that
/// diverges from the agent's inbox identity and reintroduces the name/UUID
/// dual-track bug, so refuse instead. An *unmanaged* spawn (`home = None`) has no
/// second identity track to drift against: the standalone `capture` CLI (no
/// fleet/inbox/daemon) AND TUI-local/scratch shells (never fleet members) both
/// take this branch, so a throwaway minted id is safe. The TUI shell paths pass
/// `home = None` here via `pane_factory::SpawnIdentity::UnmanagedLocalShell`.
fn resolve_spawn_instance_id(
    home: Option<&std::path::Path>,
    name: &str,
) -> anyhow::Result<crate::types::InstanceId> {
    match home {
        Some(h) => crate::fleet::resolve_uuid(h, name).ok_or_else(|| {
            anyhow::anyhow!(
                "spawn '{name}': cannot resolve InstanceId from fleet.yaml — \
                 managed instances must be registered in fleet.yaml before spawn; \
                 refusing to register the agent registry with a non-authoritative \
                 random UUID (#1441)"
            )
        }),
        None => Ok(crate::types::InstanceId::new()),
    }
}

/// Spawn an agent process and register it. Returns the authoritative
/// `InstanceId` it resolved/minted (#1441) so the caller can route the pane
/// without re-resolving (which fails for unmanaged local shells not in fleet.yaml).
pub fn spawn_agent(
    config: &SpawnConfig,
    registry: &AgentRegistry,
) -> anyhow::Result<crate::types::InstanceId> {
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

    // #1915 chokepoint: refuse to spawn an instance that is mid-delete, BEFORE
    // any side effect — `build_command` below sets the child cwd and spawns the
    // child, which re-creates `workspace/<name>`. Covers the crash-respawn
    // worker / stage2-restart / direct-spawn resurrection paths (the boot path is
    // gated earlier, in `spawn_and_register_agent`, before skills-install). The
    // deleting-set is a LEAF lock checked before any `registry.lock()` here.
    if let Some(home_path) = *home {
        if deleting::is_deleting(home_path, name) {
            anyhow::bail!(
                "#1915: refusing to spawn '{name}' — instance is mid-delete (deleting-set chokepoint)"
            );
        }
    }

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

    // #1547 M2(c): agy boot invariant. agy loads fleet MCP from
    // `<workdir>/.agents/mcp_config.json` (written by `configure_agy` BEFORE
    // spawn). If it's absent at spawn, agy boots without fleet tools AND caches
    // "no MCP" in its HOME discovery cache — the recovery-failure class #1547
    // fixes. Verify + WARN (not fatal: a bare agy is still usable manually, and
    // configure_agy already errors on its own write failure).
    if matches!(detected_backend, Some(Backend::Agy)) {
        if let Some(dir) = working_dir {
            let cfg = dir.join(".agents").join("mcp_config.json");
            if !cfg.exists() {
                tracing::warn!(
                    target: "fleet_mcp_unsupported",
                    agent = %name,
                    path = %cfg.display(),
                    "⚠️  [agy-mcp-config-missing] spawning agy but \
                     .agents/mcp_config.json is absent — fleet tools will not load \
                     and agy will cache 'no MCP'. configure_agy should have written \
                     it pre-spawn; check earlier warnings."
                );
            }
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

    let core = Arc::new(CoreMutex::new(AgentCore {
        vterm: VTerm::with_pty_writer(*cols, *rows, Arc::clone(&pty_writer)),
        subscribers: Vec::new(),
        // #1523: name the tracker at construction. Per-agent telemetry (incl. the
        // turn-completion sentinel, whose token is derived from this name) needs
        // it; `for_agent` enforces it so a future edit can't leave it empty as the
        // bare `StateTracker::new` did (r6 #2297). `name` here is the same fleet
        // instance name the instruction injection uses for `ctx.name`, so the
        // detector recomputes the SAME token the agent is told to emit.
        state: StateTracker::for_agent(detected_backend.as_ref(), name),
        health: HealthTracker::new(),
    }));

    // #1441: resolve the single authoritative instance ID (same source as inbox),
    // reused for the registry key, the PTY-reaper context, and the router-subscriber
    // lookup. Extracted to `resolve_spawn_instance_id` so the managed/unmanaged
    // identity policy is unit-testable without a live PTY spawn.
    let instance_id = resolve_spawn_instance_id(config.home, name)?;

    // Register in registry
    {
        let mut reg = registry.lock();
        reg.insert(
            instance_id,
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
    rollback.mark_registered(instance_id);

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
    let dismiss = prepare_dismiss_patterns(&dismiss);
    let shutdown_for_reaper = shutdown.clone();
    let deleted_for_reaper = {
        let reg = registry.lock();
        reg.get(&instance_id)
            .map(|h| Arc::clone(&h.deleted))
            .unwrap_or_default()
    };
    let n = name.to_string();
    let ctx = PtyReadContext {
        name: n.clone(),
        instance_id,
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
    // need the file contents injected as the first user message on Idle.
    if let Some(b) = detected_backend.as_ref() {
        let preset = b.preset();
        if preset.inject_instructions_on_ready {
            if let Some(dir) = working_dir {
                // Read the instructions body here — while we hold the spawn
                // context and before the `Idle` poll window starts — so an
                // external process mutating the file between write and
                // bootstrap cannot inject a different prompt. Skip the
                // bootstrap entirely if the file is missing/empty.
                let path = dir.join(preset.instructions_path);
                match std::fs::read_to_string(&path) {
                    Ok(content) if !content.trim().is_empty() => {
                        spawn_instructions_bootstrap(
                            Arc::clone(registry),
                            instance_id,
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
        if let Some(handle) = reg.get(&instance_id) {
            let (tx, rx) = crossbeam_channel::bounded(1024);
            // Lock order: registry → core (registry held here, core acquired
            // under it = the canonical direction). `core` is a short,
            // non-self-IPC temporary (subscriber push only) dropped on this
            // statement, so it neither trips the #1492/#1535 self-IPC-under-lock
            // guard (`sync_audit::assert_no_registry_lock_for_self_ipc`, which
            // fail-fasts any self-IPC entered while CORE_LOCK_DEPTH>0) nor needs
            // a snapshot-Arc: there is no reverse core→registry path in-tree
            // (the supervisor inversion #1593 killed; the hot tick loop pinned
            // by `tick_does_not_reacquire_registry_under_core_f2`, #1530/F2), so
            // this AB-BA pair cannot form. See docs/DAEMON-LOCK-ORDERING.md.
            handle.core.lock().subscribers.push(tx);
            crate::daemon::router::register_agent(name, rx);
        }
    }

    // Disarm the rollback guard — all ordered mutations succeeded.
    rollback.commit();

    tracing::info!(agent = name, backend = backend_command, args = %config.args.join(" "), "spawned");
    Ok(instance_id)
}

/// #CR-2026-06-14: lock the per-agent core (OFF the registry lock path) and
/// report whether it has reached Idle. Kept as a helper so the readiness check
/// snapshots `Arc::clone(&h.core)` under the registry lock, drops that guard, and
/// only then acquires the core lock here — never registry→core nested.
fn bootstrap_core_is_idle(core: &std::sync::Arc<CoreMutex<AgentCore>>) -> bool {
    core.lock().state.get_state() == crate::state::AgentState::Idle
}

/// Poll until the agent reaches Idle, then inject the pre-read instructions
/// content as a first user message. Used by backends (Kiro) whose CLI does
/// not auto-load the steering file.
///
/// The `content` is captured at spawn time (see call site) rather than
/// re-read after Ready: this closes the mutation window where an external
/// process could swap the instructions file between write and inject.
/// Poll until the agent reaches Idle (or timeout / shutdown / agent-gone), then
/// settle and snapshot its [`InjectTarget`] with the registry lock released.
/// `None` => do not inject. Shared by the Kiro instructions bootstrap and the
/// fresh-restart self-kick — both "wait for the freshly-spawned session to reach
/// the prompt, then type one first turn" (injecting while the backend is still
/// `Starting` would be swallowed).
///
/// #CR-2026-06-14 (concurrency): snapshot the core Arc under the tier-1 registry
/// lock, DROP the registry guard, THEN lock the core (in `bootstrap_core_is_idle`)
/// — never nest the per-agent core lock inside the registry lock. The old
/// registry→core nesting established an acquisition order a core→registry path
/// could deadlock against, every 200ms at startup.
fn wait_for_idle_inject_target(
    registry: &AgentRegistry,
    instance_id: crate::types::InstanceId,
    name: &str,
    timeout: std::time::Duration,
    shutdown: Option<&Arc<std::sync::atomic::AtomicBool>>,
    what: &str,
) -> Option<InjectTarget> {
    let deadline = std::time::Instant::now() + timeout;
    let poll_interval = std::time::Duration::from_millis(200);
    loop {
        if let Some(s) = shutdown {
            if s.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!(agent = %name, what, "bootstrap timed out waiting for Idle");
            return None;
        }
        let core = {
            let reg = registry.lock();
            match reg.get(&instance_id) {
                Some(h) => std::sync::Arc::clone(&h.core),
                None => return None, // agent gone
            }
        };
        if bootstrap_core_is_idle(&core) {
            break;
        }
        std::thread::sleep(poll_interval);
    }
    // Small settle delay so the prompt is fully painted before we type.
    std::thread::sleep(std::time::Duration::from_millis(500));
    // #1530/F1: snapshot the inject target under the registry lock, release it,
    // THEN inject (caller side) — never hold the registry across the blocking write.
    let reg = registry.lock();
    reg.get(&instance_id).map(InjectTarget::from_handle)
}

fn spawn_instructions_bootstrap(
    registry: AgentRegistry,
    instance_id: crate::types::InstanceId,
    name: String,
    content: String,
    timeout: std::time::Duration,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    let thread_name = format!("{name}_instr_boot");
    // fire-and-forget: instruction-bootstrap thread polls Idle then injects the
    // snapshotted instructions content. Observes shutdown inside the poll loop.
    // JoinHandle dropped — short-lived; one missed bootstrap on shutdown is cosmetic.
    let spawn_result = std::thread::Builder::new().name(thread_name).spawn(move || {
        let _census = crate::thread_census::register("instr_boot"); // M3: was "pty_reader"
        // force=true bootstrap → use the (non-gated) target inject directly.
        if let Some(tgt) = wait_for_idle_inject_target(
            &registry,
            instance_id,
            &name,
            timeout,
            shutdown.as_ref(),
            "instructions",
        ) {
            if let Err(e) = inject_with_target(&tgt, content.as_bytes()) {
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

/// fresh-restart SELF-KICK. After a `restart_instance mode=fresh` respawn the new
/// session sits idle with no first turn — nothing drives the agent to recover its
/// in-flight state, so an operator-absent overnight restart silently strands the
/// fleet (the recurring "lead restarted and just sat there" failure). This polls
/// the freshly-spawned session to Idle (so the inject isn't swallowed while the
/// backend is still `Starting`) and injects a single `[AGEND-RESUME]`
/// self-bootstrap first turn, armed for ≤1 re-delivery via the inject-delivery
/// verifier. Fired ONLY from the SPAWN handler when `restart_spawn_params` set the
/// independent `self_kick_on_ready` flag (fresh restart) — the flag is NEVER
/// derived from `SpawnMode::Fresh` (initial fleet spawns are Fresh too, and must
/// not self-kick).
pub(crate) fn spawn_self_kick_bootstrap(
    registry: AgentRegistry,
    instance_id: crate::types::InstanceId,
    name: String,
    timeout: std::time::Duration,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    let thread_name = format!("{name}_selfkick");
    // fire-and-forget: mirrors spawn_instructions_bootstrap's lifetime/discipline.
    let spawn_result = std::thread::Builder::new().name(thread_name).spawn(move || {
        let _census = crate::thread_census::register("self_kick");
        let prompt = fresh_restart_self_kick_prompt();
        if let Some(tgt) = wait_for_idle_inject_target(
            &registry,
            instance_id,
            &name,
            timeout,
            shutdown.as_ref(),
            "self-kick",
        ) {
            // force=true: we already waited for Idle; the submit key drives the turn.
            match inject_with_target(&tgt, prompt.as_bytes()) {
                Ok(()) => {
                    // Re-delivery verification (≤1 redeliver, operator-visible if
                    // undelivered) so a swallowed first turn can't silently strand
                    // the restart. arm is a no-op for non-hook backends.
                    crate::daemon::inject_delivery::arm(&name, &prompt);
                    // #t-81376 Phase-0 shadow: this RESUME self-kick IS a daemon
                    // recovery turn — shadow-arm the expectation so the supervisor
                    // gap arm can later tell a failed recovery turn from a normal
                    // Idle. No-op unless AGEND_RECOVERY_SHADOW=1; `()` → inert (D3).
                    crate::daemon::recovery_shadow::arm_expectation(&name);
                    tracing::info!(agent = %name, "fresh-restart self-kick injected");
                }
                Err(e) => {
                    tracing::warn!(agent = %name, error = %e, "fresh-restart self-kick inject failed");
                }
            }
        }
    });
    if let Err(e) = spawn_result {
        tracing::warn!(error = %e, "failed to spawn self-kick bootstrap thread");
    }
}

/// Context for PTY read loop reaper (reduces argument count).
struct PtyReadContext {
    name: String,
    /// #1441: authoritative registry key resolved once at spawn. The reaper
    /// uses it for all registry lookups; `name` is kept for name-keyed side
    /// channels (ipc port, heartbeat, event log, metadata, router, api).
    instance_id: crate::types::InstanceId,
    core: Arc<CoreMutex<AgentCore>>,
    pty_writer: PtyWriter,
    registry: AgentRegistry,
    home: Option<std::path::PathBuf>,
    crash_tx: Option<CrashChannel>,
    dismiss_patterns: Vec<PreparedDismissPattern>,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
    deleted: Arc<std::sync::atomic::AtomicBool>,
}

/// PTY read loop: feeds VTerm, broadcasts output, auto-dismisses dialogs, handles exit.
/// Broadcast one PTY output chunk to all subscribers WITHOUT blocking.
///
/// The caller holds the agent's `core.lock()` (the broadcast is kept atomic
/// with `feed_with_fg` so a concurrent `subscribe_with_dump` can't interleave a
/// dump between process and broadcast). That makes blocking here lethal: a
/// blocking `send` on a full `bounded(1024)` subscriber channel would hold the
/// core lock forever and wedge every core-lock waiter — the main TUI
/// render/input thread, the supervisor, all of it. (Observed: two agents'
/// pty_read threads parked in a full-channel send while holding their core
/// locks; the TUI drains those very channels but was itself parked waiting for a
/// core lock — a deadlock cycle that froze the whole daemon.)
///
/// `try_send` never blocks. On `Full` the consumer is too far behind, so the
/// chunk is dropped (best-effort mirror; the consumer resyncs from the next
/// screen dump) and `dropped_chunks` is bumped + throttled-logged. On
/// `Disconnected` the subscriber is removed (the `retain` returns `false`).
fn broadcast_pty_output(
    subscribers: &mut Vec<crossbeam_channel::Sender<Vec<u8>>>,
    data: &[u8],
    dropped_chunks: &mut u64,
    agent: &str,
) {
    subscribers.retain(|tx| match tx.try_send(data.to_vec()) {
        Ok(()) => true,
        Err(crossbeam_channel::TrySendError::Full(_)) => {
            *dropped_chunks += 1;
            // Throttle: first drop, then powers-of-two, so a chronically-full
            // subscriber stays visible in the log without flooding it.
            if dropped_chunks.is_power_of_two() {
                tracing::warn!(
                    agent,
                    dropped_chunks = *dropped_chunks,
                    "pty broadcast: subscriber channel full — dropping output chunk (consumer \
                     stalled). Mirror is best-effort; the daemon is NOT blocked (was a freeze \
                     before this guard)."
                );
            }
            true
        }
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
    });
}

fn pty_read_loop(
    pty_reader: &mut dyn Read,
    ctx: &PtyReadContext,
    mut capture: Box<dyn crate::capture::CaptureWriter + Send>,
) {
    let PtyReadContext {
        name,
        instance_id,
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
    let mut dismiss_scan_enabled = !dismiss_patterns.is_empty();
    // #t-23: debug-only seam — verbose per-read PTY logging (read counts / byte
    // totals). Off by default; enable with `AGEND_DEBUG_PTY_READ=1`. Tightened
    // from presence-based (`is_ok()`: any value, even `=0`, enabled it) to the
    // literal `"1"` so the value is meaningful.
    let debug_reads = std::env::var("AGEND_DEBUG_PTY_READ").as_deref() == Ok("1");
    let mut read_count: u64 = 0;
    let mut total_bytes: u64 = 0;
    // #1492-class: count subscriber chunks dropped because a consumer's bounded
    // channel was full. Throttled-logged so a chronically-stalled subscriber is
    // observable without flooding (see the broadcast site below).
    let mut dropped_chunks: u64 = 0;

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
                let (screen, state_changed, dismiss_latch_off) = {
                    let mut c = core.lock();
                    // Disjoint field borrows so the lazy-fg closure may read
                    // `vterm` while `state` is borrowed mutably (both fields of
                    // the same guarded `AgentCore`).
                    let AgentCore {
                        vterm,
                        state,
                        subscribers,
                        ..
                    } = &mut *c;
                    vterm.process(data);
                    let rows = vterm.rows() as usize;
                    // #perf-R1: hash the CHEAP de-wrapped text-only tail for the
                    // unchanged-frame dedup gate and build the per-char fg colour
                    // mask LAZILY — only on a dedup MISS. A redraw flood (Ink /
                    // spinner re-emitting an identical frame) thus skips the
                    // O(rows*cols) `classify_fg` rebuild + per-row allocations
                    // while holding the contended core lock. `tail_lines_dewrapped`
                    // is byte-identical to `tail_lines_with_fg().0`, so the dedup
                    // decision and the post-render dismiss scan below are unchanged.
                    //
                    // #1450: on a MISS the HIGH_FP colour anchor still reads the
                    // mask off the resolved grid cells (alacritty has normalized
                    // 16/256/truecolor SGR) via `tail_lines_with_fg().1`.
                    let screen = vterm.tail_lines_dewrapped(rows);
                    let state_changed =
                        state.feed_with_lazy_fg(&screen, || vterm.tail_lines_with_fg(rows).1);
                    let dismiss_latch_off = state_changed
                        && (state.get_state() == crate::state::AgentState::Idle
                            || state.has_productive_output());
                    broadcast_pty_output(subscribers, data, &mut dropped_chunks, name);
                    (screen, state_changed, dismiss_latch_off)
                };

                let in_cooldown = dismiss_cooldown_until
                    .map(|t| std::time::Instant::now() < t)
                    .unwrap_or(false);
                if dismiss_scan_enabled
                    && state_changed
                    && !in_cooldown
                    && try_prepared_dismiss_dialog(name, &screen, pty_writer, dismiss_patterns)
                {
                    dismiss_cooldown_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(10));
                }
                if dismiss_latch_off {
                    dismiss_scan_enabled = false;
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
    handle_pty_close(
        name,
        instance_id,
        registry,
        home,
        crash_tx,
        shutdown,
        deleted,
    );
}

/// Sprint 21 F-NEW1: kill the process tree rooted at the agent's child PID,
/// if still registered. Looks up the PID through the registry → child mutex
/// chain, then delegates to [`crate::process::kill_process_tree`] (PR-U #158).
///
/// No-op if the agent is not in the registry (already cleaned up) or if the
/// child has no live PID (already reaped). Idempotent on dead PIDs — safe to
/// call multiple times during the same crash-detection sequence.
fn sweep_child_tree(id: &crate::types::InstanceId, registry: &AgentRegistry) {
    let pid: Option<u32> = {
        let reg = registry.lock();
        reg.get(id).and_then(|h| h.child.lock().process_id())
    };
    if let Some(pid) = pid {
        crate::process::kill_process_tree(pid);
    }
}

/// Handle PTY close: determine if crash, graceful exit, or daemon shutdown.
/// Exit classification for handle_pty_close dispatch.
// pub(crate) + derives so the daemon respawn tests can assert on the real
// `classify_exit` output instead of re-implementing the match inline.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExitKind {
    UserExit,
    SignalKill,
    Crash,
}

/// #1744-H5: should an external signal-kill of `name` escalate a leaderless-team
/// P0? Fail-closed via `self_orch_status` (`Yes`|`Unknown` fire, `No` skip).
/// `home == None` can't read the teams config → can't determine self-orch → skip
/// (a SignalKill normally carries a home; `None` is an extreme / test path).
fn signal_kill_self_orch_should_escalate(home: &Option<std::path::PathBuf>, name: &str) -> bool {
    home.as_ref()
        .map(|h| crate::teams::self_orch_status(h, name) != crate::teams::SelfOrchStatus::No)
        .unwrap_or(false)
}

pub(crate) fn classify_exit(exit_code: Option<i32>) -> ExitKind {
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
            // CR-2026-06-14: a never-observed exit (no code within the 2s poll
            // window) is reached when the daemon force-kills / sweeps a wedged
            // process tree — NOT a spontaneous crash. Classifying it as Crash
            // drove a respawn of a process the daemon deliberately tore down.
            // Treat it as a SignalKill (daemon-induced teardown), which is not
            // respawned.
            tracing::warn!("process didn't exit in 2s — treating as signal-kill (daemon teardown), not a respawnable crash");
            ExitKind::SignalKill
        }
    }
}

fn wait_for_process_exit(
    name: &str,
    id: &crate::types::InstanceId,
    registry: &AgentRegistry,
) -> Option<i32> {
    for _ in 0..20 {
        let reg = registry.lock();
        if reg.get(id).is_none() {
            tracing::debug!(agent = name, "not in registry, skipping crash handling");
            return Some(0);
        }
        if let Some(handle) = reg.get(id) {
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

fn cleanup_agent(
    name: &str,
    id: &crate::types::InstanceId,
    registry: &AgentRegistry,
    home: &Option<std::path::PathBuf>,
) {
    registry.lock().remove(id);
    if let Some(ref home) = home {
        crate::ipc::remove_port(&crate::daemon::run_dir(home), name);
    }
}

/// Pure startup-grace decision, split out of [`is_startup_failure`] so it is
/// unit-testable without a live registry / PTY-backed `AgentHandle` (which can't
/// be constructed in a unit test). An exit is a startup FAILURE — meaning NO
/// shell fallback — when the agent has been up < 5s AND no user input arrived
/// since it was spawned. `uptime` is `(elapsed, spawned_at_epoch_ms)` for the
/// agent, or `None` when it is absent from the registry.
fn startup_failure_from(uptime: Option<(std::time::Duration, u64)>, last_input_at_ms: u64) -> bool {
    match uptime {
        // Input at/after the spawn epoch is a real user session → not a startup
        // failure. Input timestamped before the spawn epoch is stale (a prior
        // session) and must NOT suppress the startup-failure path.
        Some((elapsed, spawned_at_epoch_ms)) => {
            elapsed < std::time::Duration::from_secs(5) && last_input_at_ms < spawned_at_epoch_ms
        }
        None => false,
    }
}

fn is_startup_failure(name: &str, id: &crate::types::InstanceId, registry: &AgentRegistry) -> bool {
    let uptime = {
        let reg = registry.lock();
        reg.get(id)
            .map(|h| (h.spawned_at.elapsed(), h.spawned_at_epoch_ms))
    };
    let last_input_at_ms = crate::daemon::heartbeat_pair::snapshot_for(name).last_input_at_ms;
    startup_failure_from(uptime, last_input_at_ms)
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
    id: &crate::types::InstanceId,
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
        reg.get(id)
            .map(|h| {
                // registry → core order; `c` is a short non-self-IPC temporary
                // (vterm dims read) dropped at the closure end. Safe per the
                // unidirectional registry→core invariant — no reverse path
                // in-tree (#1593 killed it; runtime guard #1492/#1535) — so no
                // snapshot-Arc. See the spawn-site note + docs/DAEMON-LOCK-ORDERING.md.
                let c = h.core.lock();
                (c.vterm.cols(), c.vterm.rows())
            })
            .unwrap_or_else(|| crossterm::terminal::size().unwrap_or((120, 40)))
    };

    let work_dir: Option<std::path::PathBuf> = home.as_ref().and_then(|h| {
        let meta_path = crate::agent_ops::metadata_path_resolved(h, name);
        std::fs::read_to_string(meta_path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|v| {
                v["working_directory"]
                    .as_str()
                    .map(std::path::PathBuf::from)
            })
    });

    cleanup_agent(name, id, registry, home);

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
        Ok(_) => {
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

fn on_crash_exit(
    name: &str,
    id: &crate::types::InstanceId,
    registry: &AgentRegistry,
    crash_tx: &Option<CrashChannel>,
) {
    tracing::info!(agent = name, "setting restarting state");
    {
        let reg = registry.lock();
        if let Some(handle) = reg.get(id) {
            // registry → core order; `core` is a short non-self-IPC temporary
            // (set_restarting only) dropped on this statement. Safe per the
            // unidirectional registry→core invariant — no reverse path in-tree
            // (#1593 killed it; runtime guard #1492/#1535) — so no snapshot-Arc.
            // See the spawn-site note + docs/DAEMON-LOCK-ORDERING.md.
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
    id: &crate::types::InstanceId,
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
        cleanup_agent(name, id, registry, home);
        return;
    }

    tracing::info!(agent = name, "PTY closed, waiting for process exit");
    // #t-41673 gap-instrument: time the per-agent exit wait so a slow child reap
    // is attributable in the restart-freeze breakdown. Pure tracing.
    let exit_wait_started = std::time::Instant::now();
    let exit_code = wait_for_process_exit(name, id, registry);
    tracing::info!(
        agent = name,
        exit_wait_ms = exit_wait_started.elapsed().as_millis() as u64,
        "process exit wait complete"
    );
    sweep_child_tree(id, registry);

    if deleted.load(std::sync::atomic::Ordering::SeqCst) {
        tracing::info!(agent = name, "agent deleted, skipping shell fallback");
        return;
    }

    match classify_exit(exit_code) {
        ExitKind::UserExit => {
            if is_startup_failure(name, id, registry) {
                on_startup_failure(name, home, crash_tx);
            } else {
                on_clean_exit_shell_fallback(
                    name, id, exit_code, registry, home, crash_tx, shutdown,
                );
            }
        }
        ExitKind::SignalKill => {
            // #1744-H5: an external signal-kill (OOM exit 137 / SIGTERM 143) of a
            // self-orchestrator is a SILENT leaderless death — cleanup_agent just
            // removes it, emitting no crash event / notify. A graceful daemon
            // shutdown (`is_shutdown`) and a deleted agent both early-returned
            // above, so this branch only ever sees an UNEXPECTED external kill of
            // a live agent. Escalate a P0 before cleanup. Fail-closed via
            // self_orch_status (Yes|Unknown fire, No skip); home=None can't
            // determine self-orch → skip.
            if signal_kill_self_orch_should_escalate(home, name) {
                tracing::error!(
                    agent = name,
                    exit_code = exit_code.unwrap_or(0),
                    "#1744-H5: self-orchestrator killed by external signal — escalating P0"
                );
                let msg = format!(
                    "🛑 Self-orchestrator `{name}` was killed by an external signal (OOM / \
                     SIGKILL, exit {code}) and removed — it will NOT be auto-respawned \
                     (a signal-kill is not a crash). Its team is leaderless and no peer can \
                     relay: manual operator intervention is required.",
                    code = exit_code.unwrap_or(0),
                );
                crate::channel::notify_all_escalation_channels(
                    name,
                    crate::channel::NotifySeverity::Error,
                    &msg,
                    false,
                );
            }
            cleanup_agent(name, id, registry, home);
        }
        ExitKind::Crash => {
            on_crash_exit(name, id, registry, crash_tx);
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
    write_to_pty(&agent.pty_writer, data)
}

/// #1530/F1: write to a pre-captured [`PtyWriter`] (`Arc::clone(&h.pty_writer)`),
/// so the caller can release the registry lock BEFORE this blocking write
/// (`write_with_timeout` waits up to `PTY_WRITE_TIMEOUT`). Same delivery as
/// [`write_to_agent`].
pub(crate) fn write_to_pty(writer: &PtyWriter, data: &[u8]) -> crate::error::Result<()> {
    write_with_timeout(writer, data).map_err(crate::error::AgendError::PtyWrite)?;
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

/// Inject `text` (plus the backend submit key) into an agent's PTY, via a
/// pre-captured [`InjectTarget`] snapshot + agent `name`, so the caller can
/// release the registry lock BEFORE this (potentially multi-second) write runs.
///
/// `force` exempts the inject from the #1513 busy-gate. Pass `true` for
/// recovery, operator, and drain injects that MUST reach the PTY: the
/// ServerRateLimit retry, bootstrap instructions, and the api INJECT handler
/// (which is both the operator-inject entry AND the notification-queue drain
/// convergence, so gating it would re-defer an already-flushed item). Pass
/// `false` for daemon-originated direct injects (cron and schedule replay) that
/// should yield to a mid-generation or mid-keystroke pane.
///
/// #1530/F1: this snapshot form replaced the old `inject_to_agent(&AgentHandle)`
/// — callers must `InjectTarget::from_handle(h)` UNDER the registry lock, `drop`
/// the guard, THEN call this — so the registry is never held across the (up to
/// 5s `recv_timeout` + payload-scaled sleep) blocking write. The busy-gate's
/// agent-state read is the lock-free on-disk snapshot (never the per-agent core
/// lock, #1492).
/// #1769: marker token prefixed to daemon-self-originated AUTO injects (e.g. the
/// ServerRateLimit "continue" retry). The full prefix is
/// `[AGEND-AUTO kind=<kind>] ` — a sibling of the `[AGEND-MSG]` inbox header, so
/// an orchestrator agent can tell a daemon auto-nudge apart from a real operator
/// instruction (a bare injected "continue" was mistaken for an operator command
/// and acted on). Agents are taught (see `instructions.rs` / FLEET-DEV-PROTOCOL)
/// to treat an `[AGEND-AUTO]` line as a low-priority resume signal — continue
/// in-progress work, NEVER dispatch from it.
pub(crate) const DAEMON_AUTO_INJECT_MARKER: &str = "[AGEND-AUTO";

/// #1769: build the `[AGEND-AUTO kind=<kind>] ` prefix for an auto-inject.
fn daemon_auto_prefix(kind: &str) -> String {
    format!("{DAEMON_AUTO_INJECT_MARKER} kind={kind}] ")
}

/// fresh-restart self-kick marker — DISTINCT from [`DAEMON_AUTO_INJECT_MARKER`].
/// `[AGEND-AUTO]` is a low-priority resume NUDGE the agent must NEVER act on (the
/// test-pinned blanket rule in `instructions.rs`); `[AGEND-RESUME]` is the
/// OPPOSITE — an actionable SELF-bootstrap trigger telling a just-fresh-restarted
/// agent to recover its OWN in-flight state. The two markers stay separate (a new
/// marker, NOT an `[AGEND-AUTO]` per-kind carve-out) so neither rule's meaning is
/// muddied: `[AGEND-AUTO]` = never act, `[AGEND-RESUME]` = run your recovery.
pub(crate) const DAEMON_RESUME_INJECT_MARKER: &str = "[AGEND-RESUME]";

/// #2090 O1: progress-backstop marker — DISTINCT from both [`DAEMON_AUTO_INJECT_MARKER`]
/// and [`DAEMON_RESUME_INJECT_MARKER`]. Like `[AGEND-RESUME]` (and unlike the
/// never-act `[AGEND-AUTO]`), `[AGEND-PROGRESS]` is **actionable**: it asks the
/// agent to post a brief progress update on its in-flight external-channel
/// request. It is still daemon-originated and NOT operator authority — the agent
/// must not dispatch a task or make a decision from it beyond posting the update.
/// A separate marker (NOT an `[AGEND-AUTO]` per-kind carve-out, mirroring the
/// `[AGEND-RESUME]` design) so the `[AGEND-AUTO]` "never act" blanket stays clean:
/// the report-mode backstop must be acted on, so it can't wear the never-act tag.
pub(crate) const DAEMON_PROGRESS_INJECT_MARKER: &str = "[AGEND-PROGRESS]";

/// #2282: context-handoff marker — a FOURTH distinct daemon token, actionable like
/// [`DAEMON_RESUME_INJECT_MARKER`] / [`DAEMON_PROGRESS_INJECT_MARKER`] and unlike the
/// never-act [`DAEMON_AUTO_INJECT_MARKER`]. The context-handoff watchdog
/// (`daemon::per_tick::context_handoff`) injects it near context-full to ask the
/// agent to SAVE its state (write `SESSION-HANDOFF.md` + annotate its task) before
/// the context window runs out. It must NOT use `[AGEND-AUTO]`: that marker's "never
/// act" blanket would suppress the very save the nudge asks for — the latent bug
/// this fixes (the nudge was silently ignored, leaving only the 92% operator
/// escalation as a degraded backstop). A separate marker keeps the `[AGEND-AUTO]`
/// blanket clean (mirrors the `[AGEND-RESUME]` / `[AGEND-PROGRESS]` design).
pub(crate) const DAEMON_HANDOFF_INJECT_MARKER: &str = "[AGEND-HANDOFF]";

/// The fixed first turn injected ONCE after a fresh-restart respawn. It is an
/// actionable SELF-bootstrap trigger (recover the agent's OWN in-flight state),
/// NOT an operator command and NOT authority to dispatch new work. Ordering per
/// the design review (must-follow ③): the task board + `list_instances` are the
/// AUTHORITATIVE live sources; `SESSION-HANDOFF.md` is only a stale-tolerant hint
/// because a fresh restart's DELETE=kill may have happened before any fresh
/// handoff was written.
pub(crate) fn fresh_restart_self_kick_prompt() -> String {
    format!(
        "{DAEMON_RESUME_INJECT_MARKER} You were just fresh-restarted and lost your in-memory \
         context. Recover your OWN state now, in this order: (1) rebuild your in-flight picture \
         from the AUTHORITATIVE live sources — the task board (your claimed/assigned tasks) and \
         list_instances (peers + any dangling sub-agents); (2) drain your inbox; (3) read \
         SESSION-HANDOFF.md as a STALE-TOLERANT hint only — if it is missing or looks out of \
         date, trust the board/inbox over it; (4) execute pending handoff TODOs and reconnect \
         dangling sub-agents, then resume normal work. This is a self-bootstrap trigger to \
         recover YOUR OWN state — it is NOT an operator command and NOT authority to dispatch \
         new work."
    )
}

pub(crate) fn inject_with_target_gated(
    target: &InjectTarget,
    name: &str,
    text: &[u8],
    force: bool,
    auto_kind: Option<&str>,
) -> crate::error::Result<()> {
    // #1769: daemon self-originated auto-injects carry an identifying marker so
    // an orchestrator can distinguish them from real operator/peer input. The
    // marker is prepended HERE (before both the deferred-enqueue and the direct
    // inject) so the tag survives whichever delivery path runs. Worker semantics
    // are unchanged — the inner payload + submit are preserved; only a leading
    // text tag is added. `None` (operator relay / api INJECT / inbox — which
    // already carry their own headers) injects verbatim.
    let marked: Vec<u8>;
    let text: &[u8] = match auto_kind {
        Some(kind) => {
            marked = [daemon_auto_prefix(kind).as_bytes(), text].concat();
            &marked
        }
        None => text,
    };
    // #1513 PR-2: gate direct PTY injects like the notification path. Self-
    // contained via AGEND_HOME; AGEND_HOME absent (non-daemon / unit test) →
    // gate skipped.
    if !force {
        if let Ok(home) = std::env::var("AGEND_HOME") {
            let home = std::path::Path::new(&home);
            if crate::inbox::notify::should_defer_direct_inject(home, name) {
                // Gated direct injects (cron / replay) are UTF-8 text wakes;
                // enqueue ambient-class for the per-tick flush to drain once the
                // pane settles (the flush re-injects via the api INJECT path,
                // landing back here with force=true — byte-equivalent delivery).
                // #1630: this enqueue IS the deferred-delivery path — if it
                // fails the wake is lost outright. The fn returns Result and the
                // callers handle it (e.g. daemon::replay logs "replay inject
                // failed"), so propagate rather than swallow. anyhow→AgendError
                // has no generic variant, so map through ApiError (message
                // preserved via Display).
                // #t-3558 P2: an AGEND-AUTO nudge (auto_kind set) routes through
                // the coalescing enqueue so a non-draining agent can't stack
                // identical same-kind retry nudges (keep-latest). Everything else
                // keeps the plain ambient enqueue.
                let text_str = String::from_utf8_lossy(text);
                let enq = if auto_kind.is_some() {
                    crate::notification_queue::enqueue_coalesced_auto(home, name, &text_str)
                } else {
                    crate::notification_queue::enqueue_classified(home, name, &text_str, false)
                };
                return enq.map_err(|e| {
                    crate::error::AgendError::ApiError(format!("deferred enqueue: {e}"))
                });
            }
        }
    }
    inject_with_target(target, text)
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
    // #1912: gate the pre-submit wait on the backend's input-widget style.
    if target.typed_inject {
        // Readback-confirm (#1912): poll the RENDERED input area until the typed
        // line's tail-sentinel appears, THEN submit. Replaces the fixed-sleep
        // "guess" that racing codex's re-rendering `›` widget required (every codex
        // version re-tuned the magic number). FAIL-OPEN: on timeout we submit
        // anyway (the helper warns) — this is the agent-wake lifeline, so an
        // unconfirmed readback must never become "don't submit" (= agent never wakes).
        let _confirmed = readback_confirm_typed(target, &stripped);
    } else {
        // claude `❯` bulk fast path — tolerates bulk bytes + `\r`; keep byte-identical.
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    write_with_timeout(&target.pty_writer, submit)?;
    // #1912: post-submit observability (log/metric-only, NEVER retries — a second
    // `\r` would risk double-submit). Typed-inject only; bulk's fast path stays lean.
    if target.typed_inject {
        let _submitted = observe_post_submit(target);
    }
    Ok(())
}

/// #1912: tail-sentinel of a (possibly multi-line) injected payload — the last
/// run of up to `MAX` chars on the final non-empty line, the line the submit `\r`
/// commits. Short + drawn from the bottom line so it stays robust to input-box
/// wrapping. Empty when the payload has no non-blank line (nothing to confirm).
fn inject_sentinel(stripped: &str) -> String {
    const MAX: usize = 24;
    let last_line = stripped
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let take_from = last_line
        .char_indices()
        .rev()
        .take(MAX)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    last_line[take_from..].to_string()
}

const READBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const READBACK_POLL: std::time::Duration = std::time::Duration::from_millis(15);
const POSTSUBMIT_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
/// Bottom rows scanned for the input area — the prompt + a few wrapped input rows.
const READBACK_TAIL_ROWS: usize = 8;

/// #1912: poll the rendered input area until the typed line's tail-sentinel
/// renders, then return `true`. Returns `false` (FAIL-OPEN: caller submits anyway,
/// this warns) if unconfirmed within the timeout. Acquires the core lock only
/// briefly per poll (read `tail_lines`, drop) so the `pty_read_loop` renders the
/// backend's echo of the typed chars between polls.
fn readback_confirm_typed(target: &InjectTarget, stripped: &str) -> bool {
    readback_confirm_typed_with(target, stripped, READBACK_TIMEOUT, READBACK_POLL)
}

fn readback_confirm_typed_with(
    target: &InjectTarget,
    stripped: &str,
    timeout: std::time::Duration,
    poll: std::time::Duration,
) -> bool {
    let sentinel = inject_sentinel(stripped);
    if sentinel.is_empty() {
        return true; // nothing to confirm (empty/whitespace payload)
    }
    let start = std::time::Instant::now();
    let mut polls = 0u32;
    loop {
        if target.deleted.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let visible = target.core.lock().vterm.tail_lines(READBACK_TAIL_ROWS);
        if visible.contains(&sentinel) {
            tracing::debug!(
                tag = "#1912-readback-confirmed",
                polls,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "typed inject line rendered in input area before submit"
            );
            return true;
        }
        if start.elapsed() >= timeout {
            tracing::warn!(
                tag = "#1912-readback-timeout",
                elapsed_ms = start.elapsed().as_millis() as u64,
                sentinel_len = sentinel.len(),
                "typed inject line not confirmed in input area within timeout — submitting anyway (fail-open)"
            );
            return false;
        }
        std::thread::sleep(poll);
        polls += 1;
    }
}

/// #1912: post-submit observability (no retry). A successful submit clears the
/// input line / grows the transcript, so the rendered tail CHANGES; returns `true`
/// the moment it does. If it stays byte-identical for `POSTSUBMIT_WINDOW`, the
/// submit likely didn't take — warn (log/metric only) and return `false`. NEVER
/// retries the submit: a second `\r` would risk double-submit.
fn observe_post_submit(target: &InjectTarget) -> bool {
    observe_post_submit_with(target, POSTSUBMIT_WINDOW, READBACK_POLL)
}

fn observe_post_submit_with(
    target: &InjectTarget,
    window: std::time::Duration,
    poll: std::time::Duration,
) -> bool {
    let before = target.core.lock().vterm.tail_lines(READBACK_TAIL_ROWS);
    let start = std::time::Instant::now();
    loop {
        if target.deleted.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        std::thread::sleep(poll);
        if target.core.lock().vterm.tail_lines(READBACK_TAIL_ROWS) != before {
            return true;
        }
        if start.elapsed() >= window {
            tracing::warn!(
                tag = "#1912-postsubmit-nochange",
                "input area unchanged after submit — a readback-confirmed line may not have submitted"
            );
            return false;
        }
    }
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
    /// #1912: the agent's core, so the typed-inject readback-confirm can poll the
    /// RENDERED input line (`core.vterm.tail_lines`) before sending the submit key —
    /// without re-acquiring the registry lock (which the snapshot exists to release).
    /// Polled with brief lock-and-release so the `pty_read_loop` renders the echo
    /// between polls.
    pub core: Arc<CoreMutex<AgentCore>>,
}

impl InjectTarget {
    pub fn from_handle(h: &AgentHandle) -> Self {
        Self {
            pty_writer: Arc::clone(&h.pty_writer),
            inject_prefix: h.inject_prefix.clone(),
            submit_key: h.submit_key.clone(),
            typed_inject: h.typed_inject,
            deleted: Arc::clone(&h.deleted),
            core: Arc::clone(&h.core),
        }
    }
}

/// Send a message to a named agent via direct registry injection.
/// Returns true if the agent was found and injected.
pub fn send_to_registry(
    registry: &AgentRegistry,
    home: &std::path::Path,
    from: &str,
    target: &str,
    text: &str,
) -> bool {
    // #1441: resolve name → UUID via the single authoritative resolver so
    // injection identity matches inbox identity (cannot drift).
    let Some(target_id) = crate::fleet::resolve_uuid(home, target) else {
        return false;
    };
    let target_snapshot = {
        let reg = lock_registry(registry);
        match reg.get(&target_id) {
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
            // #1441: registry is UUID-keyed; the display name lives on the
            // handle. Filter `exclude` and the returned names off `handle.name`.
            .filter(|(_id, handle)| {
                (exclude != Some(handle.name.as_str()))
                    && crate::backend::Backend::from_command(&handle.backend_command).is_some()
            })
            .map(|(_id, handle)| (handle.name.to_string(), InjectTarget::from_handle(handle)))
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

/// Minimal managed `AgentHandle` over a throwaway `true` PTY, for cross-module
/// tests that need a registry entry whose CONTENTS are irrelevant — e.g. the
/// t-65 register-external TOCTOU repro only needs `reg.contains_key(id)` to be
/// true. Spawns a short-lived `true`; the caller owns cleanup of the registry.
/// `unix`-gated: the `true` binary (and this PTY pattern) is unix-only, matching
/// the sibling `true`-backed handle tests.
#[cfg(all(test, unix))]
pub(crate) fn mk_test_handle(name: &str, id: crate::types::InstanceId) -> AgentHandle {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new("true");
    cmd.cwd(std::env::temp_dir());
    let child = pair.slave.spawn_command(cmd).expect("spawn true");
    drop(pair.slave);
    let pty_writer: PtyWriter = Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
    let pty_master: Arc<Mutex<Box<dyn MasterPty + Send>>> = Arc::new(Mutex::new(pair.master));
    let core = Arc::new(CoreMutex::new(AgentCore {
        vterm: VTerm::with_pty_writer(80, 24, Arc::clone(&pty_writer)),
        subscribers: Vec::new(),
        state: StateTracker::new(None),
        health: HealthTracker::new(),
    }));
    AgentHandle {
        id,
        name: name.to_string().into(),
        backend_command: "true".to_string(),
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
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
mod review_repro_agent_binding;
