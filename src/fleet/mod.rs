pub(crate) mod merge;
pub(crate) mod persist;
pub(crate) mod resolve;
pub(crate) mod watchdog;

#[allow(unused_imports)]
pub use watchdog::WatchdogConfig;

#[allow(unused_imports)]
pub use persist::{
    add_instance_to_yaml, add_instances_to_yaml, add_team_to_yaml, migrate_teams_json_to_yaml,
    remove_instance_from_yaml, remove_instances_from_yaml, remove_team_from_yaml,
    update_channel_telegram_group_id, update_instance_field, update_team_in_yaml, TeamWriteOutcome,
};

use crate::backend::Backend;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Mtime-based cache for `FleetConfig::load()`.
/// Avoids repeated `read_to_string` + `serde_yaml_ng` parse when the
/// file hasn't changed on disk.
static FLEET_CACHE: std::sync::Mutex<Option<FleetCacheEntry>> = std::sync::Mutex::new(None);

struct FleetCacheEntry {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
    /// #perf-R4: store the parsed config behind an `Arc` so a cache HIT in
    /// [`FleetConfig::load_arc`] is a refcount bump, not a deep clone of the
    /// whole `instances`/`teams` map under the global `FLEET_CACHE` mutex.
    config: Arc<FleetConfig>,
}

fn invalidate_cache() {
    let mut guard = FLEET_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// Single source of truth for the fleet configuration filename.
pub const FLEET_YAML_FILENAME: &str = "fleet.yaml";

/// Canonical path to fleet.yaml given a home directory.
pub fn fleet_yaml_path(home: &Path) -> PathBuf {
    home.join(FLEET_YAML_FILENAME)
}

/// #1441: single authoritative `name` → `InstanceId` resolution from
/// fleet.yaml. Both inbox path resolution and the agent registry route
/// through this one function so live-process identity (PTY inject / pane
/// subscription) and inbox identity share one source and cannot drift.
/// Returns `None` when the instance is absent from fleet.yaml or has no
/// parseable id.
pub fn resolve_uuid(home: &Path, name: &str) -> Option<crate::types::InstanceId> {
    FleetConfig::load(&fleet_yaml_path(home))
        .ok()
        .and_then(|c| {
            c.instances
                .get(name)
                .and_then(|i| i.id.as_deref())
                .and_then(crate::types::InstanceId::parse)
        })
}

/// #1488: is `name` a currently-known fleet instance? True iff fleet.yaml has
/// an `instances:` entry for it (regardless of whether it has a parseable id or
/// is currently running). Unlike [`resolve_uuid`], this does not require an id
/// — an offline-but-configured instance counts as known. Used by the cron
/// schedule fail-safe and the boot orphan sweep to distinguish a deletable
/// ghost target from a legitimate offline instance.
pub fn instance_is_known(home: &Path, name: &str) -> bool {
    FleetConfig::load(&fleet_yaml_path(home))
        .map(|c| c.instances.contains_key(name))
        .unwrap_or(false)
}

/// #1491: the orchestrator (lead) of the first team that lists `member`.
/// Used by the handoff-timeout watchdog to escalate an unclaimed CI handoff
/// to the right lead.
pub fn team_orchestrator_for(home: &Path, member: &str) -> Option<String> {
    FleetConfig::load(&fleet_yaml_path(home))
        .ok()
        .and_then(|c| {
            c.teams
                .values()
                .find(|t| t.members.iter().any(|m| m == member))
                .and_then(|t| t.orchestrator.clone())
        })
}

/// #1989: the fleet.yaml schema version this daemon reads and writes. Bump
/// ONLY on a breaking (non-additive) change — additive optional fields with
/// serde defaults do NOT bump it (`docs/COMPATIBILITY.md`).
pub const FLEET_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FleetConfig {
    /// #1989: fleet.yaml schema version. Omitted = version 1 (every
    /// pre-#1989 file). `Option` (not `u32` + serde default fn) so the
    /// derived `Default` and the serde default can't disagree — both land
    /// on `None`, resolved to 1 by [`FleetConfig::effective_schema_version`].
    /// A version newer than [`FLEET_SCHEMA_VERSION`] WARNs at load (fields
    /// this daemon doesn't know are silently dropped by serde) instead of
    /// refusing — a refuse would brick the daemon on a hand-edit typo.
    /// Compatibility policy: `docs/COMPATIBILITY.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    #[serde(default)]
    pub defaults: InstanceDefaults,
    /// #2477: symbolic model tiers (e.g. `cheap`, `strong`) resolved to the
    /// concrete backend model string passed as `--model`. This keeps role/task
    /// policy in fleet.yaml while preserving the existing low-level `model:`
    /// escape hatch for exact model names.
    #[serde(default)]
    pub model_tiers: HashMap<String, String>,
    /// #2477: default model tier by typed role. Instance-level `model:` /
    /// `model_tier:` still win; this is the fleet-level role policy knob.
    #[serde(default)]
    pub role_model_tiers: HashMap<RoleKind, String>,
    #[serde(default)]
    pub instances: HashMap<String, InstanceConfig>,
    #[serde(default)]
    pub teams: HashMap<String, TeamConfig>,
    /// #1440: fleet-wide env keys to pass through to every agent backend when
    /// `AGEND_ENV_ISOLATION` is on (additive with per-instance
    /// `passthrough_env`). Still gated by `is_sensitive_env_key`, so listing
    /// `LD_PRELOAD` etc. has no effect. Use for corp-specific, non-secret env
    /// like `NODE_EXTRA_CA_CERTS` / `SSL_CERT_FILE`.
    #[serde(default)]
    pub passthrough_env: Vec<String>,
    /// Channel configuration (e.g., Telegram). Legacy singular form.
    ///
    /// Prefer [`FleetConfig::channels`] (plural) going forward — per
    /// `docs/archived/PLAN-channel-abstraction.md` §3.6. When both are omitted,
    /// Telegram stays off. When only `channels:` is set, `normalize()`
    /// collapses the first entry into this field so existing call sites
    /// (which read `self.channel` directly) keep working unchanged.
    pub channel: Option<ChannelConfig>,
    /// Named channel configurations. Each key is a user-chosen name
    /// (e.g. `tg-main`, `discord-ops`) and the value follows the same
    /// tagged `type: telegram` shape as the singular form. Multi-channel
    /// routing is wired in a later PR; for now, this is a parser-level
    /// extension that normalizes back into [`FleetConfig::channel`].
    #[serde(default)]
    pub channels: Option<HashMap<String, ChannelConfig>>,
    /// Template definitions for batch deployment.
    #[serde(default)]
    pub templates: Option<HashMap<String, serde_yaml_ng::Value>>,
    /// #790 Option 1: IANA timezone name (e.g. `Asia/Taipei`) used to
    /// render UTC timestamps on human-facing surfaces (TUI overlays,
    /// `[ci-watch-stalled]` notification bodies). `None` falls back to
    /// `chrono::Local` (system tz), preserving the pre-#790 behaviour
    /// from Sprint 54 P2-6. Storage timestamps stay UTC unconditionally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_timezone: Option<String>,
    /// #1547 (A): base directory under which the daemon creates a NON-hidden
    /// link to each agy instance's real (hidden) `$AGEND_HOME/workspace/<name>`
    /// dir. agy rejects any workspace whose path has a dot-prefixed ancestor
    /// (`is hidden: ignore uri`), so the daemon points agy's `$PWD` at
    /// `<base>/<name>` (a link) while keeping its CWD at the real allowed-root
    /// workspace. `None` → default `<user_home>/agend-ws`. Only consulted for
    /// the agy backend; other backends ignore it. See `crate::agy_workspace`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agy_workspace_link_base: Option<PathBuf>,
    /// Watchdog topology — which agent the idle watchdog watches and who receives
    /// each watchdog / anti-stall / decision-timeout notification. Replaces five
    /// `AGEND_*` env vars (now a deprecated fallback). Omitted block → built-in
    /// defaults, byte-identical to the pre-migration behaviour. See
    /// [`watchdog::WatchdogConfig`].
    #[serde(default)]
    pub watchdog: WatchdogConfig,
    #[serde(skip)]
    pub(crate) home: Option<PathBuf>,
}

/// An entry in a channel's `user_allowlist`. **Channel-agnostic by design** —
/// used by Telegram today, and intended for any future channel adapter
/// (Discord, Slack, …): when their inbound handlers land they should give their
/// `user_allowlist` this same `Option<Vec<AllowlistEntry>>` type and resolve the
/// sender name the same way (see `TelegramState::username_for` +
/// `NotifySource::Channel`, which already renders `user:NAME via {channel}` for
/// any `ChannelKind`).
///
/// Accepts either a bare numeric user id (legacy: `- 12345`) or a `{ id, name }`
/// map that also records a display name. The name is surfaced as
/// `[user:NAME via <channel>]` in agent inboxes when the sender has no public
/// platform username (so the operator no longer shows up as `unknown`).
/// Backward compatible via `#[serde(untagged)]` — old bare-id lists still parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AllowlistEntry {
    /// Bare Telegram user id (legacy shape).
    Id(i64),
    /// `{ id: 12345, name: "Alice" }` — id plus display name.
    Named { id: i64, name: String },
}

impl AllowlistEntry {
    /// The Telegram user id, regardless of shape.
    pub fn id(&self) -> i64 {
        match self {
            Self::Id(id) | Self::Named { id, .. } => *id,
        }
    }

    /// The configured display name, if this entry carries one.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Named { name, .. } => Some(name.as_str()),
            Self::Id(_) => None,
        }
    }
}

impl From<i64> for AllowlistEntry {
    fn from(id: i64) -> Self {
        Self::Id(id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChannelConfig {
    #[serde(rename = "telegram")]
    Telegram {
        /// Env var name containing the bot token. Defaults to
        /// `AGEND_TELEGRAM_BOT_TOKEN`; falls back to legacy `AGEND_BOT_TOKEN`
        /// with a deprecation warning.
        #[serde(default = "default_telegram_bot_token_env")]
        bot_token_env: String,
        /// Telegram group chat ID.
        group_id: i64,
        /// Mode: "topic" for forum topics.
        #[serde(default = "default_mode")]
        mode: String,
        /// Optional allowlist of Telegram user IDs (`user.id`, not username)
        /// permitted to command the fleet via messages.
        ///
        /// - `None` (field omitted): **legacy open mode** — any group member
        ///   is accepted; a deprecation warning is logged on startup.
        /// - `Some([])` (explicit empty list): reject all — useful to lock
        ///   down an environment without removing the channel config.
        /// - `Some([...])`: only those user IDs are accepted; others are
        ///   dropped with a warn log.
        ///
        /// Each entry is either a bare id (`- 12345`) or `{ id, name }` — see
        /// [`AllowlistEntry`]. A configured `name` is shown as the sender in
        /// agent inboxes (`[user:NAME via telegram]`) when the Telegram account
        /// has no public @username.
        #[serde(default)]
        user_allowlist: Option<Vec<AllowlistEntry>>,
        /// Optional fleet-activity binding — where cross-instance
        /// `FleetEvent`s (delegate / report / decision / broadcast) are
        /// mirrored as one-liner log rows. Omitted = no fleet sink for
        /// this channel; the producer registry in `src/mcp/handlers.rs`
        /// still emits events, but nothing routes them to Telegram.
        ///
        /// PR-A lands the schema only; resolution into a concrete
        /// `BindingRef` and the rendering pipeline land with PR-B (see
        /// `docs/archived/DESIGN-stage-b-ux.md` §3 and §5).
        #[serde(default)]
        fleet_binding: Option<FleetBindingConfig>,
    },
    /// Discord adapter (Phase 2+). Bootstrap selection lands in Phase 1;
    /// actual adapter implementation is behind the `discord` feature gate.
    #[serde(rename = "discord")]
    Discord {
        /// Env var name containing the Discord bot token.
        #[serde(default = "default_discord_bot_token_env")]
        bot_token_env: String,
        /// Discord guild (server) ID.
        guild_id: u64,
    },
}

/// Where fleet activity gets mirrored on a channel. Accepts two YAML
/// forms for operator ergonomics:
///
/// - **Struct** — `{ type: topic, name: "fleet-activity" }`. Canonical
///   form. `type` picks the platform primitive (Telegram forum topic,
///   Discord channel, Slack thread, …) and `name` is the
///   human-readable identifier.
/// - **String shorthand** — `"#agend-ops"`. Convenience form for
///   Discord / Slack where the binding is simply "this named channel".
///   Telegram does not use the shorthand today (topics are created by
///   name, not by `#tag`), so the Telegram adapter will warn and
///   ignore string-form bindings when it tries to resolve them in
///   PR-B.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FleetBindingConfig {
    /// Canonical struct form — platform-tagged binding descriptor.
    Struct(FleetBindingStruct),
    /// Shorthand: a bare channel / tag string. Non-Telegram adapters
    /// (Discord, Slack) interpret this as the target channel name.
    Shorthand(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FleetBindingStruct {
    /// Telegram forum topic / Discord channel-equivalent. `name` is the
    /// display name used to find or create the topic; resolution (map
    /// name → topic_id) happens at bootstrap, not at parse time.
    Topic { name: String },
}

fn default_mode() -> String {
    "topic".to_string()
}

fn default_telegram_bot_token_env() -> String {
    "AGEND_TELEGRAM_BOT_TOKEN".to_string()
}

fn default_discord_bot_token_env() -> String {
    "AGEND_DISCORD_BOT_TOKEN".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceDefaults {
    /// Backend preset name (e.g., "claude", "kiro-cli").
    pub backend: Option<Backend>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub model: Option<String>,
    /// Symbolic model tier key looked up in `model_tiers` (#2477).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_tier: Option<String>,
    pub ready_pattern: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub instructions: Option<String>,
}

/// #1563: per-instance idle policy. `Active` (default) is a worker expected to
/// make steady progress — the idle watchdog tracks it and the supervisor may
/// escalate a silent `Starting`/startup-prose stall to the operator. `OnDemand`
/// is a coordinator/responder (e.g. `general`) that is legitimately quiet
/// between requests: it is exempt from idle-watchdog tracking and from the two
/// `Starting`-context stall-forward paths, WITHOUT touching the real
/// permission/interactive-prompt escalation (#1552 still fires for it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdleExpectation {
    #[default]
    Active,
    OnDemand,
}

/// #2344: typed agent-role selector. Drives the per-role MCP tool subset
/// (`crate::mcp::registry::tool_subset_for_role`). Distinct from the free-text
/// `role` description below: the operator DECLARES this typed value (no brittle
/// prose-parsing — see archived plan PLAN-role-enum-default-by-role-2026-04).
///
/// `#[serde(rename_all = "snake_case")]` → fleet.yaml values: `reviewer`,
/// `planner`, `explorer`, `orchestrator`, `implementer`, `utility`, `proxy`.
/// STRICT (#2344 D2): a `role_kind:` present with any other value FAILS
/// fleet.yaml load (serde unknown-variant error naming the offending instance +
/// value); an ABSENT `role_kind` is legal and resolves to the all-open default
/// (opt-in — no existing fleet breaks, no real agent loses tools).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleKind {
    /// Restricted read/report subset — no instance/worktree lifecycle (#2158).
    Reviewer,
    /// Restricted read/report — same subset surface as `Reviewer`.
    Planner,
    /// Strictest read-only — also drops `repo` (checkout) + `ci` (run/dispatch).
    Explorer,
    /// Full capability — fleet orchestration/dispatch (lead).
    Orchestrator,
    /// Full capability — implements changes (dev).
    Implementer,
    /// Full capability — general/utility agent.
    Utility,
    /// Full capability — bridges an external/proxy backend.
    Proxy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceConfig {
    /// Role description. TS version uses "description", accepted as alias.
    #[serde(alias = "description")]
    pub role: Option<String>,
    /// #2344: typed role selector driving the per-role MCP tool subset (opt-in;
    /// absent → all-open). Distinct from the free-text `role` description above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_kind: Option<RoleKind>,
    /// Unique instance ID (UUIDv4). Auto-assigned on first load if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Backend preset name — overrides defaults.backend.
    pub backend: Option<Backend>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub working_directory: Option<String>,
    /// Sprint 54 P1-B Bug 2 fix Option A: source repository path used by
    /// `dispatch_auto_bind_lease` when creating per-agent worktrees via
    /// `git worktree add`. Decouples "where the agent's git history
    /// lives" (this field) from `working_directory` ("where the agent's
    /// state/home dir lives"), which the daemon auto-writes to a
    /// per-agent stub workspace at spawn time. When absent, the
    /// worktree-leasing path falls back to `working_directory` for
    /// backward compatibility — operators can hand-edit fleet.yaml to
    /// point at a real source repo (e.g. operator clone) per agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_repo: Option<String>,
    /// Sprint 55 P0-B EC4 — explicit GitHub `owner/name` override for non-
    /// GitHub remotes (where `parse_github_owner_repo` would return `None`)
    /// or fork/upstream disambiguation. When present, takes precedence over
    /// derivation from `source_repo`'s origin. When absent, daemon falls
    /// back to `derive_repo_from_remote(source_repo)` per existing behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub ready_pattern: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// #1440: per-instance env keys to pass through under `AGEND_ENV_ISOLATION`
    /// (additive with fleet-level [`FleetConfig::passthrough_env`]). Still
    /// `is_sensitive_env_key`-gated.
    #[serde(default)]
    pub passthrough_env: Vec<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub topic_id: Option<i32>,
    /// Custom git branch name for worktree. TS version uses "worktree_source".
    #[serde(alias = "worktree_source")]
    pub git_branch: Option<String>,
    /// Per-instance worktree opt-out. `None` or `Some(true)` = auto-create
    /// worktree (default, per §10.4). `Some(false)` = skip worktree creation,
    /// instance works in main repo working tree. Use for orchestrators and
    /// reviewers who never commit.
    #[serde(default)]
    pub worktree: Option<bool>,
    /// Model override (e.g., "opus", "sonnet"). Passed as --model flag.
    pub model: Option<String>,
    /// Symbolic model tier key looked up in `FleetConfig::model_tiers` (#2477).
    /// `model` wins when both are set; use this for cheap/strong policy knobs
    /// without hard-coding backend model IDs on every instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_tier: Option<String>,
    /// Display name for UI/Telegram.
    pub display_name: Option<String>,
    /// Path to extra instructions file (relative to fleet.yaml dir).
    /// Content is appended to the generated agent instructions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Sprint 56 Track F (#496): operator-controlled GitHub username for
    /// this instance. Lets `task_sweep`'s authorship gate compare
    /// `pr.author_login` against the right namespace — without this
    /// mapping, the gate compared GitHub user names against agend
    /// instance names (e.g. `cheerc` vs `dev-lead`) and silently
    /// rejected every cross-namespace mismatch. When `None`, the sweep
    /// falls back to direct string compare for backwards compatibility
    /// with deployments where instance name happens to equal the
    /// operator's GitHub login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_login: Option<String>,
    /// Sprint 61 W1 PR-2 (#P0-2): per-instance skills allowlist. When
    /// `Some(vec)`, the daemon's auto-install hook installs only the
    /// listed skill names from `<home>/skills/` into this agent's
    /// per-backend skill paths. When `None`, every skill in the unified
    /// source is installed (preserves the W1 PR-1 #585 default behavior).
    /// `Some(vec![])` is meaningful — explicitly opts the agent OUT of
    /// all skills (no per-backend dirs are populated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    /// Custom skills path override. When present, the daemon pulls skills
    /// from this path instead of `<home>/skills`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_path: Option<String>,
    /// #991: topic binding mode — controls whether a Telegram topic is
    /// created at spawn time. `None` or `Some("auto")` = current behavior.
    /// `Some("skip")` = no topic ever. `Some("deferred")` = no topic at
    /// spawn, operator can retrofit later via `bind_topic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_binding_mode: Option<String>,
    /// Per-instance idle timeout in seconds. When set, the idle watchdog
    /// uses this instead of the global `dev_idle_threshold_secs`. Allows
    /// reviewers to have tighter timeouts than dev agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default = "default_true")]
    pub idle_watchdog_enabled: bool,
    /// #1563: idle policy. `Active` (default) tracks the agent in the idle
    /// watchdog and arms `Starting`-context stall escalation. `OnDemand` exempts
    /// a legitimately-quiet coordinator (e.g. `general`) from idle-watchdog noise
    /// and from the two `Starting`-FP stall-forward paths, while preserving the
    /// #1552 real-prompt escalation. Omitted → `Active` (zero migration).
    #[serde(default)]
    pub idle_expectation: IdleExpectation,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConfig {
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub orchestrator: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Sprint 54 fleet-yaml unification: team creation timestamp, preserved
    /// from the runtime `teams.json` lineage. Optional so existing
    /// fleet.yaml templates without the field still parse — runtime team
    /// creation stamps it; operator-edited fleet.yaml entries may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_repo: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept_from: Vec<String>,
}

impl FleetConfig {
    /// #1440: env keys to pass through to instance `name` under
    /// `AGEND_ENV_ISOLATION` — fleet-level ∪ per-instance, deduplicated.
    /// Still `is_sensitive_env_key`-gated at injection time.
    pub fn resolve_passthrough_env(&self, name: &str) -> Vec<String> {
        let mut keys = self.passthrough_env.clone();
        if let Some(inst) = self.instances.get(name) {
            keys.extend(inst.passthrough_env.iter().cloned());
        }
        keys.sort();
        keys.dedup();
        keys
    }

    /// #perf-R4: cache-shared load returning `Arc<FleetConfig>`. On a cache HIT
    /// this is an `Arc::clone` (refcount bump), NOT a deep clone of the whole
    /// `FleetConfig` (its `instances`/`teams` HashMaps). The supervisor per-tick
    /// hot callers (`idle_expectation_for`, `pane_input_backend_supported`,
    /// `metadata_path_resolved`) use this so a 10s tick over N agents doesn't
    /// deep-copy the fleet ~4×/agent while holding the global `FLEET_CACHE`
    /// mutex (which render/etc. also contend). Cold/infrequent callers keep
    /// [`FleetConfig::load`] (owned, deep-cloned) — behaviour-identical.
    pub fn load_arc(path: &Path) -> Result<Arc<Self>> {
        let meta = std::fs::metadata(path).ok();
        let disk_mtime = meta.as_ref().and_then(|m| m.modified().ok());
        let disk_size = meta.as_ref().map(|m| m.len());

        if let (Some(mtime), Some(size)) = (disk_mtime, disk_size) {
            let guard = FLEET_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = guard.as_ref() {
                if entry.path == path && entry.mtime == mtime && entry.size == size {
                    return Ok(Arc::clone(&entry.config));
                }
            }
        }

        let config = Arc::new(Self::load_uncached(path)?);

        if let (Some(mtime), Some(size)) = (disk_mtime, disk_size) {
            let mut guard = FLEET_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(FleetCacheEntry {
                path: path.to_path_buf(),
                mtime,
                size,
                config: Arc::clone(&config),
            });
        }

        Ok(config)
    }

    /// Owned load (deep-cloned `FleetConfig`). Byte-identical to the pre-#perf-R4
    /// behaviour for the ~190 cold/infrequent call sites that want an owned
    /// value. Per-tick hot callers use [`FleetConfig::load_arc`] to skip the
    /// deep clone.
    pub fn load(path: &Path) -> Result<Self> {
        Ok((*Self::load_arc(path)?).clone())
    }

    /// #1989: resolved schema version — an omitted `schema_version:` means 1.
    pub fn effective_schema_version(&self) -> u32 {
        self.schema_version.unwrap_or(1)
    }

    fn load_uncached(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read fleet config: {}", path.display()))?;
        let mut config: FleetConfig = serde_yaml_ng::from_str(&content)
            .with_context(|| format!("Failed to parse fleet config: {}", path.display()))?;
        if config.effective_schema_version() > FLEET_SCHEMA_VERSION {
            tracing::warn!(
                file_version = config.effective_schema_version(),
                supported = FLEET_SCHEMA_VERSION,
                path = %path.display(),
                "fleet.yaml schema_version is newer than this daemon supports — \
                 unknown fields are silently ignored and the config may be \
                 misread; upgrade the daemon (docs/COMPATIBILITY.md)"
            );
        }
        config.normalize();
        config.home = path.parent().map(|p| p.to_path_buf());
        // Sprint 46 P1: backfill instance IDs + reserved-name warnings
        config.backfill_ids(path);
        Ok(config)
    }

    /// Normalize legacy configs so that `backend:` is the single source of
    /// truth for "what runs in the pane". When only the legacy `command:`
    /// field is set, derive a [`Backend`] from it (presets like `claude` land
    /// on the matching variant; `/bin/bash` or similar land on
    /// [`Backend::Shell`]; arbitrary paths land on [`Backend::Raw`]).
    ///
    /// The `command:` field itself is left intact for backward compatibility
    /// with call sites that still read it directly — follow-up commits
    /// collapse those paths and eventually remove the field.
    fn normalize(&mut self) {
        if self.defaults.backend.is_none() {
            if let Some(cmd) = &self.defaults.command {
                self.defaults.backend = Some(Backend::parse_str(cmd));
            }
        }
        for inst in self.instances.values_mut() {
            if inst.backend.is_none() {
                if let Some(cmd) = &inst.command {
                    inst.backend = Some(Backend::parse_str(cmd));
                }
            }
        }

        // Channel dual-accept: if the user wrote only `channels:` (plural
        // map), collapse the first entry — sorted by name for determinism
        // — into the legacy `channel:` field so existing call sites keep
        // working unchanged. Multi-channel routing is wired in a later PR;
        // until then, we pick one and log a warning when more than one is
        // declared. When `channel:` is already set, plural is a no-op and
        // runtime behavior is byte-identical to today.
        if self.channel.is_none() {
            if let Some(map) = self.channels.as_ref() {
                let mut names: Vec<&String> = map.keys().collect();
                names.sort();
                if let Some(first) = names.first().copied() {
                    if names.len() > 1 {
                        tracing::warn!(
                            count = names.len(),
                            picked = %first,
                            "fleet.yaml declares {} channels but multi-channel routing \
                             is not yet wired; using first entry by name. Follow-up PR \
                             in T1 will merge inbound streams across all channels.",
                            names.len(),
                        );
                    }
                    if let Some(cfg) = map.get(first) {
                        self.channel = Some(cfg.clone());
                    }
                }
            }
        }
    }

    /// Resolve an instance config by merging with defaults + backend preset.
    ///
    /// `backend` is the single source of truth for preset behavior: its variant
    /// determines args / ready_pattern / submit_key (Shell / Raw variants have
    /// empty presets). An explicit `command:` field, if present, still overrides
    /// the binary path to spawn — useful for users pointing a preset at a
    /// custom-built binary (`backend: claude` + `command: /opt/claude-v2/claude`).
    pub fn resolve_instance(&self, name: &str) -> Option<ResolvedInstance> {
        resolve::resolve_instance(self, name)
    }

    /// Get all instance names.
    /// Sprint 46 P1: assign UUIDv4 IDs to instances that lack them.
    /// Writes back to fleet.yaml unless AGEND_FLEET_NO_AUTO_MIGRATE=1.
    ///
    /// #965 (B): the locked write path re-reads the on-disk doc under
    /// `mutate_fleet_yaml`'s flock so concurrent peer mutations are
    /// preserved. After the locked write, the assigned IDs are mirrored
    /// back into `self.instances` so callers' in-memory FleetConfig
    /// matches what's on disk (a separate `resolve_instance` thread
    /// otherwise reads a different ID and self-send / identity checks
    /// break).
    fn backfill_ids(&mut self, fleet_path: &std::path::Path) {
        let template_names: std::collections::HashSet<String> = self
            .templates
            .as_ref()
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();

        // #1186: Name collision detection — emit error once, not per-tick WARN.
        static COLLISION_REPORTED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        for name in self.instances.keys() {
            if template_names.contains(name)
                && !COLLISION_REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                tracing::error!(
                    name,
                    "fleet.yaml: instance name collides with template name — \
                     rename one to avoid routing ambiguity"
                );
            }
        }

        let needs_backfill = self.instances.values().any(|i| i.id.is_none());
        if !needs_backfill || std::env::var("AGEND_FLEET_NO_AUTO_MIGRATE").as_deref() == Ok("1") {
            return;
        }

        let home = match fleet_path.parent() {
            Some(p) => p,
            None => {
                tracing::warn!("backfill_ids: fleet_path has no parent; skipping write");
                return;
            }
        };

        // Under the flock: re-read disk, assign missing IDs on the fresh
        // view (preserves concurrent peer additions / deletions), write
        // back. Capture the (name → id) assignments so we can mirror them
        // into the in-memory self.
        let mut assigned: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let assigned_ref = &mut assigned;
        if let Err(e) = persist::mutate_fleet_yaml(home, "", |doc| {
            if let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) {
                for (name_value, inst_value) in instances.iter_mut() {
                    let inst_map = match inst_value.as_mapping_mut() {
                        Some(m) => m,
                        None => continue,
                    };
                    let id_key = serde_yaml_ng::Value::String("id".to_string());
                    let needs_id = inst_map.get(&id_key).map(|v| v.is_null()).unwrap_or(true);
                    if needs_id {
                        let id = crate::types::InstanceId::new();
                        let name = name_value.as_str().unwrap_or("?").to_string();
                        inst_map.insert(id_key, serde_yaml_ng::Value::String(id.full()));
                        tracing::info!(
                            name = %name,
                            id = %id.short(),
                            "[fleet-migration] assigned instance ID (under flock)"
                        );
                        assigned_ref.insert(name, id.full());
                    }
                }
            }
            Ok(!assigned_ref.is_empty())
        }) {
            tracing::warn!(error = %e, "backfill_ids: locked write failed");
            return;
        }

        // Sync in-memory self.instances with the IDs we just persisted so
        // a concurrent `resolve_instance` call returning a different
        // FleetConfig instance still sees the same identity. Without
        // this, the in-memory caller would carry a different (or absent)
        // id than the one a sibling thread reads from disk.
        for (name, id) in assigned {
            if let Some(inst) = self.instances.get_mut(&name) {
                inst.id = Some(id);
            }
        }
    }

    pub fn instance_names(&self) -> Vec<String> {
        self.instances.keys().cloned().collect()
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ResolvedInstance {
    pub name: String,
    pub backend_command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub working_directory: Option<PathBuf>,
    pub ready_pattern: Option<String>,
    pub submit_key: String,
    pub role: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub topic_id: Option<i32>,
    pub git_branch: Option<String>,
    pub model: Option<String>,
    pub worktree: Option<bool>,
    pub instructions: Option<String>,
    /// Sprint 54 P1-B Bug 2 fix: resolved source repository path. See
    /// `InstanceConfig::source_repo` for semantics; this is the
    /// `PathBuf` form after fleet.yaml string deserialization, used
    /// directly by `dispatch_auto_bind_lease` and friends.
    pub source_repo: Option<PathBuf>,
    /// Sprint 55 P0-B EC4: optional GitHub `owner/name` override copied
    /// through from `InstanceConfig::repo`. See that field for semantics.
    pub repo: Option<String>,
}

/// Entry for adding a dynamic instance to fleet.yaml.
#[derive(Default)]
pub struct InstanceYamlEntry {
    pub backend: Option<String>,
    pub working_directory: Option<String>,
    pub role: Option<String>,
    pub instructions: Option<String>,
    /// Sprint 54 P1-B Bug 2 fix: optional source-repo path written
    /// into the fleet.yaml stanza. Daemon auto-write callers leave
    /// this `None` (gradient deployment per general's constraint —
    /// only operator hand-edits opt agents in). Callers that DO want
    /// to seed a default at write time set it explicitly.
    pub source_repo: Option<String>,
    /// Sprint 55 P0-B EC4: optional explicit `owner/name` override per
    /// `InstanceConfig::repo`. Daemon auto-write callers leave this
    /// `None` (gradient deployment); operator hand-edits opt agents in.
    pub repo: Option<String>,
    /// Sprint 56 Track F (#496): GitHub login mapping per
    /// `InstanceConfig::github_login`. Daemon auto-write callers leave
    /// this `None` so existing instance creation paths don't suddenly
    /// require operator-supplied GitHub identities; operator hand-edits
    /// or fleet.yaml templates opt agents in.
    pub github_login: Option<String>,
    /// Sprint 56 Track E (#450): process args mirror of
    /// [`InstanceConfig::args`]. `Some(vec![])` is meaningful (operator
    /// asks for an empty arglist explicitly); `None` means "don't
    /// override defaults". Template deployments populate from the
    /// template stanza's `args:` field; operator hand-edits round-trip
    /// through the writer.
    pub args: Option<Vec<String>>,
    /// Sprint 56 Track E (#450): backend model mirror of
    /// [`InstanceConfig::model`] (e.g. "opus", "sonnet"). Surfaces as
    /// the `--model` flag for backends that honour it.
    pub model: Option<String>,
    /// #2477: symbolic model tier mirror of [`InstanceConfig::model_tier`].
    pub model_tier: Option<String>,
    /// Sprint 56 Track E (#450): process env mirror of
    /// [`InstanceConfig::env`]. Same `Option` semantics as `args`:
    /// `None` = no override, `Some(empty)` = explicit empty map.
    pub env: Option<std::collections::HashMap<String, String>>,
    /// Sprint 56 Track E (#450): ready-state regex mirror of
    /// [`InstanceConfig::ready_pattern`]. Backends that wait for a
    /// pattern before considering the agent live read this.
    pub ready_pattern: Option<String>,
    /// Sprint 56 Track E (#450): custom command override mirror of
    /// [`InstanceConfig::command`]. Templates that need a non-backend
    /// invocation (`command: my-script.sh`) flow through this. Falls
    /// back to `backend` when unset.
    pub command: Option<String>,
    /// Sprint 56 Track E (#450): per-instance worktree opt-out mirror
    /// of [`InstanceConfig::worktree`]. Templates with reviewer /
    /// orchestrator roles that never commit set this to `Some(false)`
    /// so the worktree pool skips creation. Defaults to auto-create
    /// when `None` or `Some(true)`.
    pub worktree: Option<bool>,
    /// #991: topic binding mode. See [`InstanceConfig::topic_binding_mode`].
    pub topic_binding_mode: Option<String>,
    /// Custom skills path override.
    pub skills_path: Option<String>,
}

// Persistence, merge, and team mutation functions live in fleet::persist and fleet::merge.
// Re-exported at module level for API compatibility.

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tests.rs"]
mod tests;
