use crate::backend::Backend;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Single source of truth for the fleet configuration filename.
pub const FLEET_YAML_FILENAME: &str = "fleet.yaml";

/// Canonical path to fleet.yaml given a home directory.
pub fn fleet_yaml_path(home: &Path) -> PathBuf {
    home.join(FLEET_YAML_FILENAME)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FleetConfig {
    #[serde(default)]
    pub defaults: InstanceDefaults,
    #[serde(default)]
    pub instances: HashMap<String, InstanceConfig>,
    #[serde(default)]
    pub teams: HashMap<String, TeamConfig>,
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
        #[serde(default)]
        user_allowlist: Option<Vec<i64>>,
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
    pub ready_pattern: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceConfig {
    /// Role description. TS version uses "description", accepted as alias.
    #[serde(alias = "description")]
    pub role: Option<String>,
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
    /// #841 per-instance rate-limit recovery config. `None` → use the
    /// daemon-wide default ([`RateLimitRecoveryConfig::default`]).
    /// `Some(cfg)` lets operators override knobs (most commonly
    /// `enabled: false` to opt an agent out of the auto-recovery nudge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_recovery: Option<RateLimitRecoveryConfig>,
}

/// #841 per-instance rate-limit recovery configuration.
///
/// When an agent transitions from a transient-error state (e.g.
/// `ServerRateLimit`, `RateLimit`, `ApiError`) back to `Ready`/`Idle`
/// but then sits silent for [`recovery_after_secs`] seconds, the
/// daemon supervisor injects a single `prompt` over the PTY (via
/// `compose_aware_send`, bypassing the `[AGEND-MSG]` inbox header) to
/// nudge the agent into continuing. Single-shot per recovery cycle; a
/// [`cooldown_secs`]-second cooldown prevents repeat firing across
/// consecutive cycles.
///
/// Operator opts an instance out with `enabled: false`; field-level
/// `#[serde(default)]` lets the operator override one knob without
/// re-specifying the others.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitRecoveryConfig {
    #[serde(default = "rate_limit_recovery_default_enabled")]
    pub enabled: bool,
    #[serde(default = "rate_limit_recovery_default_observe_after_secs")]
    pub observe_after_secs: u64,
    #[serde(default = "rate_limit_recovery_default_recovery_after_secs")]
    pub recovery_after_secs: u64,
    #[serde(default = "rate_limit_recovery_default_prompt")]
    pub prompt: String,
    #[serde(default = "rate_limit_recovery_default_cooldown_secs")]
    pub cooldown_secs: u64,
}

fn rate_limit_recovery_default_enabled() -> bool {
    true
}
fn rate_limit_recovery_default_observe_after_secs() -> u64 {
    30
}
fn rate_limit_recovery_default_recovery_after_secs() -> u64 {
    60
}
fn rate_limit_recovery_default_prompt() -> String {
    "continue your prior work".to_string()
}
fn rate_limit_recovery_default_cooldown_secs() -> u64 {
    300
}

impl Default for RateLimitRecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: rate_limit_recovery_default_enabled(),
            observe_after_secs: rate_limit_recovery_default_observe_after_secs(),
            recovery_after_secs: rate_limit_recovery_default_recovery_after_secs(),
            prompt: rate_limit_recovery_default_prompt(),
            cooldown_secs: rate_limit_recovery_default_cooldown_secs(),
        }
    }
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
}

impl FleetConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read fleet config: {}", path.display()))?;
        let mut config: FleetConfig = serde_yaml_ng::from_str(&content)
            .with_context(|| format!("Failed to parse fleet config: {}", path.display()))?;
        config.normalize();
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
        let inst = self.instances.get(name)?;
        let defaults = &self.defaults;

        // Backend: instance > defaults > ClaudeCode fallback when the yaml
        // specifies neither backend nor command.
        let backend = inst
            .backend
            .clone()
            .or_else(|| defaults.backend.clone())
            .unwrap_or(Backend::ClaudeCode);
        let preset = backend.preset();

        // Command path: explicit `command:` override > backend's own path.
        // Shell resolves to $SHELL at spawn time; Raw carries a literal path.
        let backend_cmd = inst
            .command
            .clone()
            .or_else(|| defaults.command.clone())
            .unwrap_or_else(|| backend.command_string());

        // User-authored extras only. Preset args are prepended by
        // `agent::spawn_agent` — including them here would double-apply.
        let args = if !inst.args.is_empty() {
            inst.args.clone()
        } else {
            defaults.args.clone()
        };

        // Merge env: defaults first, then instance overrides
        let mut env = defaults.env.clone();
        env.extend(inst.env.clone());
        env.insert("AGEND_INSTANCE_NAME".to_string(), name.to_string());

        // Ready pattern: instance > defaults > preset (empty string for
        // Shell/Raw, which means "no ready detection").
        // User-provided patterns are validated at resolve time to reject
        // malformed regex early rather than at spawn/verify.
        let ready_pattern = inst
            .ready_pattern
            .clone()
            .or_else(|| defaults.ready_pattern.clone())
            .or_else(|| {
                if preset.ready_pattern.is_empty() {
                    None
                } else {
                    Some(preset.ready_pattern.to_string())
                }
            });
        if let Some(ref pat) = ready_pattern {
            if regex::RegexBuilder::new(pat)
                .size_limit(1 << 20)
                .build()
                .is_err()
            {
                tracing::error!(
                    instance = name,
                    pattern = pat,
                    "invalid ready_pattern regex, skipping instance"
                );
                return None;
            }
        }

        // Submit key comes straight from the backend's preset. Shell/Raw
        // default to `\r`.
        let submit_key = preset.submit_key.to_string();

        let working_directory = Some(if let Some(d) = inst.working_directory.as_ref() {
            // M2: reject path traversal (component-level check)
            let wd_path = std::path::Path::new(d);
            if wd_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                tracing::warn!(
                    name,
                    dir = d,
                    "working_directory contains '..' (path traversal rejected)"
                );
                return None;
            }
            // Expand ~ to home directory
            if let Some(rest) = d.strip_prefix("~/") {
                if let Some(home) = dirs_home() {
                    home.join(rest)
                } else {
                    PathBuf::from(d)
                }
            } else {
                PathBuf::from(d)
            }
        } else {
            // Default: $AGEND_HOME/workspace/{name}/
            crate::home_dir().join("workspace").join(name)
        });

        let cols = inst.cols.or(defaults.cols);
        let rows = inst.rows.or(defaults.rows);
        let model = inst.model.clone().or_else(|| defaults.model.clone());

        // Sprint 54 P1-B Bug 2 fix Option A: resolve source_repo into
        // a PathBuf with the same `~/` expansion treatment as
        // working_directory. None when fleet.yaml omits the field —
        // dispatch_auto_bind_lease falls back to working_directory.
        let source_repo = inst.source_repo.as_ref().map(|d| {
            if d == "~" {
                dirs_home().unwrap_or_else(|| PathBuf::from(d))
            } else if let Some(rest) = d.strip_prefix("~/") {
                if let Some(home) = dirs_home() {
                    home.join(rest)
                } else {
                    PathBuf::from(d)
                }
            } else {
                PathBuf::from(d)
            }
        });

        Some(ResolvedInstance {
            name: name.to_string(),
            backend_command: backend_cmd,
            args,
            env,
            working_directory,
            ready_pattern,
            submit_key,
            role: inst.role.clone(),
            cols,
            rows,
            topic_id: inst.topic_id,
            git_branch: inst.git_branch.clone(),
            model,
            worktree: inst.worktree,
            instructions: inst.instructions.clone(),
            source_repo,
            // Sprint 55 P0-B EC4: optional explicit GitHub `owner/name`
            // override; copied through unchanged.
            repo: inst.repo.clone(),
        })
    }

    /// Get all instance names.
    /// Sprint 46 P1: assign UUIDv4 IDs to instances that lack them.
    /// Writes back to fleet.yaml unless AGEND_FLEET_NO_AUTO_MIGRATE=1.
    fn backfill_ids(&mut self, fleet_path: &std::path::Path) {
        let mut changed = false;
        let template_names: std::collections::HashSet<String> = self
            .templates
            .as_ref()
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default();

        for (name, inst) in &mut self.instances {
            // Reserved-name warning: instance name collides with template name
            if template_names.contains(name) {
                tracing::warn!(
                    name,
                    "instance name collides with template name — may cause routing ambiguity"
                );
            }
            // Backfill ID if absent
            if inst.id.is_none() {
                let id = crate::types::InstanceId::new();
                inst.id = Some(id.full());
                tracing::info!(name, id = %id.short(), "[fleet-migration] assigned instance ID");
                changed = true;
            }
        }

        if changed && std::env::var("AGEND_FLEET_NO_AUTO_MIGRATE").as_deref() != Ok("1") {
            if let Ok(content) = serde_yaml_ng::to_string(&*self) {
                let _ = crate::store::atomic_write(fleet_path, content.as_bytes());
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

fn dirs_home() -> Option<PathBuf> {
    dirs::home_dir()
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
}

/// Atomically write a serde_yaml_ng::Value back to fleet.yaml using temp + fsync + rename.
/// Caller must hold the file lock.
fn atomic_write_yaml(home: &Path, doc: &serde_yaml_ng::Value) -> Result<()> {
    let yaml = serde_yaml_ng::to_string(doc).context("Failed to serialize fleet.yaml")?;
    let fleet_path = fleet_yaml_path(home);
    // Use the shared helper so fsync-before-rename is uniform across the
    // codebase. The previous write→rename (no fsync) left a crash window
    // where the renamed-over fleet.yaml could be truncated on power loss.
    crate::store::atomic_write(&fleet_path, yaml.as_bytes())
        .context("Failed to atomic-write fleet.yaml")
}

/// Acquire the fleet.yaml file lock via flock (auto-released on crash/drop).
///
/// Delegates to the shared helper which deliberately does NOT use
/// `truncate(true)` when opening the lock file. Truncating on every
/// acquire is never required for correctness — flock is tied to the
/// inode, not the file contents — and the project-wide review flagged it
/// as a source of confusion across call sites (fleet.rs, mcp_config.rs).
fn acquire_lock(home: &Path) -> Result<std::fs::File> {
    let lock_path = home.join(".fleet.yaml.lock");
    crate::store::acquire_file_lock(&lock_path).context("failed to acquire fleet lock")
}

/// Lock fleet.yaml, parse it, apply a mutation, and atomically write back.
fn mutate_fleet_yaml(
    home: &Path,
    default_content: &str,
    mutate: impl FnOnce(&mut serde_yaml_ng::Value) -> Result<()>,
) -> Result<()> {
    let fleet_path = fleet_yaml_path(home);
    if default_content.is_empty() && !fleet_path.exists() {
        return Ok(());
    }
    let _lock = acquire_lock(home)?;
    let content =
        std::fs::read_to_string(&fleet_path).unwrap_or_else(|_| default_content.to_string());
    let mut doc: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&content).context("Failed to parse fleet.yaml")?;
    mutate(&mut doc)?;
    atomic_write_yaml(home, &doc)
}

/// Add a new instance entry to fleet.yaml. Uses file lock + atomic write.
pub fn add_instance_to_yaml(home: &Path, name: &str, config: &InstanceYamlEntry) -> Result<()> {
    add_instances_to_yaml(home, &[(name, config)])
}

/// Sprint 58 Wave 2 PR-2 (#525-9 / #16) field categorization for the
/// 3-tier fleet.yaml merge contract. Each known field maps to one of
/// two classes; unknown fields default to `OperatorHandEdit` so
/// operator additions are preserved by default.
///
/// - `DaemonManaged`: daemon is the source of truth. On merge, daemon
///   value OVERWRITES whatever the operator may have hand-edited.
///   Hand-editing these fields is futile — daemon will rewrite on
///   next mutation. Used for daemon-derived identifiers and
///   bookkeeping fields.
/// - `OperatorHandEdit`: operator-controlled. On merge, operator's
///   value WINS over daemon-supplied value when they differ. If the
///   field is unset operator-side, daemon's value lands. If both
///   sides specify different values, the merger surfaces a 真衝突
///   error per the operator-resolved baseline rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldClass {
    DaemonManaged,
    OperatorHandEdit,
}

/// Sprint 58 Wave 2 PR-2 (#525-9 / #16): hardcoded classification.
/// Per general scope FINAL LOCK + dispatch hint, the per-field
/// metadata could live as a `#[fleet_yaml(...)]` macro attribute,
/// but the hardcoded list is simpler and keeps the merge contract
/// auditable from a single function.
///
/// The list reflects the operator-resolved baseline: daemon
/// authoritatively writes identity / bookkeeping / migration
/// fields; everything else is operator-controlled.
pub fn instance_field_class(field: &str) -> FieldClass {
    match field {
        // Daemon-derived bookkeeping: identity, telegram topic,
        // git_branch (managed by worktree lease lifecycle), env
        // (template-deployed), source_repo (binding-derived),
        // worktree opt-out flag (lease-managed).
        //
        // Note that `command` / `args` / `model` / `ready_pattern`
        // are deliberately operator-managed — operators may want to
        // override the template's defaults per-instance, and
        // template re-deploys should NOT clobber those overrides.
        "id" | "topic_id" | "git_branch" | "source_repo" => FieldClass::DaemonManaged,
        _ => FieldClass::OperatorHandEdit,
    }
}

/// Sprint 58 Wave 2 PR-2 (#525-9 / #16): 真衝突 — same operator-hand-
/// edit field has incompatible values from both sides. The merger
/// surfaces this as a recoverable error so the operator can resolve
/// manually rather than silently corrupting either intent.
#[derive(Debug, Clone)]
pub struct FieldConflict {
    pub instance: String,
    pub field: String,
    pub operator_value: String,
    pub daemon_value: String,
}

impl std::fmt::Display for FieldConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fleet.yaml merge conflict — instance `{}` field `{}`: operator has `{}`, daemon proposed `{}`. \
             Hand-edit fleet.yaml to resolve, OR delete the field to accept daemon's value.",
            self.instance, self.field, self.operator_value, self.daemon_value
        )
    }
}

/// Sprint 58 Wave 2 PR-2 (#525-9 / #16): merge a new daemon-side
/// `InstanceYamlEntry` into an existing on-disk YAML mapping per
/// the 3-tier baseline rules:
///
/// 1. **DaemonManaged field** + daemon value present →
///    OVERWRITE existing.
/// 2. **OperatorHandEdit field** + daemon value present:
///    - Existing absent → write daemon value.
///    - Existing matches daemon value → no-op.
///    - Existing differs from daemon value → 真衝突 → return
///      `Err(FieldConflict)` (caller surfaces via diff output).
/// 3. **Field absent on daemon side** → preserve existing
///    (deep-merge default; never clobber operator additions).
///
/// Returns `Ok(())` on successful merge, or `Err(FieldConflict)` for
/// the first 真衝突 detected (callers may iterate or aggregate).
pub fn merge_instance_into_existing(
    name: &str,
    existing: &mut serde_yaml_ng::Mapping,
    config: &InstanceYamlEntry,
) -> Result<(), FieldConflict> {
    use serde_yaml_ng::Value;

    // Helper: classify-then-merge for scalar string fields.
    let merge_string = |existing: &mut serde_yaml_ng::Mapping,
                        field: &str,
                        daemon_value: &Option<String>|
     -> Result<(), FieldConflict> {
        let Some(new_v) = daemon_value else {
            return Ok(()); // daemon doesn't supply, preserve existing
        };
        let class = instance_field_class(field);
        let key = Value::String(field.to_string());
        match (class, existing.get(&key)) {
            (FieldClass::DaemonManaged, _) => {
                existing.insert(key, Value::String(new_v.clone()));
                Ok(())
            }
            (FieldClass::OperatorHandEdit, None) => {
                existing.insert(key, Value::String(new_v.clone()));
                Ok(())
            }
            (FieldClass::OperatorHandEdit, Some(Value::String(old_v))) if old_v == new_v => {
                Ok(()) // no-op, identical value
            }
            (FieldClass::OperatorHandEdit, Some(old)) => {
                let old_str = match old {
                    Value::String(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                Err(FieldConflict {
                    instance: name.to_string(),
                    field: field.to_string(),
                    operator_value: old_str,
                    daemon_value: new_v.clone(),
                })
            }
        }
    };

    for (field, value) in [
        ("backend", &config.backend),
        ("working_directory", &config.working_directory),
        ("role", &config.role),
        ("instructions", &config.instructions),
        ("source_repo", &config.source_repo),
        ("repo", &config.repo),
        ("github_login", &config.github_login),
        ("model", &config.model),
        ("ready_pattern", &config.ready_pattern),
        ("command", &config.command),
    ] {
        merge_string(existing, field, value)?;
    }

    // Typed-value fields (Sprint 56 Track E): args / env / worktree.
    // Each follows the same 3-tier rule. For args (Vec<String>) and
    // env (HashMap), conflict detection compares serialized form.
    if let Some(ref args) = config.args {
        let key = Value::String("args".into());
        let new_seq: Vec<Value> = args.iter().map(|s| Value::String(s.clone())).collect();
        let new_value = Value::Sequence(new_seq.clone());
        let class = instance_field_class("args");
        match (class, existing.get(&key)) {
            (FieldClass::DaemonManaged, _) => {
                existing.insert(key, new_value);
            }
            (FieldClass::OperatorHandEdit, None) => {
                existing.insert(key, new_value);
            }
            (FieldClass::OperatorHandEdit, Some(old)) if old == &new_value => {
                // identical
            }
            (FieldClass::OperatorHandEdit, Some(old)) => {
                return Err(FieldConflict {
                    instance: name.to_string(),
                    field: "args".into(),
                    operator_value: format!("{old:?}"),
                    daemon_value: format!("{new_value:?}"),
                });
            }
        }
    }
    if let Some(ref env_map) = config.env {
        let key = Value::String("env".into());
        let mut new_env = serde_yaml_ng::Mapping::new();
        for (k, v) in env_map {
            new_env.insert(Value::String(k.clone()), Value::String(v.clone()));
        }
        let new_value = Value::Mapping(new_env.clone());
        let class = instance_field_class("env");
        match (class, existing.get(&key)) {
            (FieldClass::DaemonManaged, _) => {
                existing.insert(key, new_value);
            }
            (FieldClass::OperatorHandEdit, None) => {
                existing.insert(key, new_value);
            }
            (FieldClass::OperatorHandEdit, Some(old)) if old == &new_value => {
                // identical
            }
            (FieldClass::OperatorHandEdit, Some(old)) => {
                return Err(FieldConflict {
                    instance: name.to_string(),
                    field: "env".into(),
                    operator_value: format!("{old:?}"),
                    daemon_value: format!("{new_value:?}"),
                });
            }
        }
    }
    if let Some(worktree) = config.worktree {
        let key = Value::String("worktree".into());
        let new_value = Value::Bool(worktree);
        let class = instance_field_class("worktree");
        match (class, existing.get(&key)) {
            (FieldClass::DaemonManaged, _) => {
                existing.insert(key, new_value);
            }
            (FieldClass::OperatorHandEdit, None) => {
                existing.insert(key, new_value);
            }
            (FieldClass::OperatorHandEdit, Some(old)) if old == &new_value => {
                // identical
            }
            (FieldClass::OperatorHandEdit, Some(old)) => {
                return Err(FieldConflict {
                    instance: name.to_string(),
                    field: "worktree".into(),
                    operator_value: format!("{old:?}"),
                    daemon_value: format!("{new_value:?}"),
                });
            }
        }
    }

    Ok(())
}

/// Build a fresh instance mapping from an `InstanceYamlEntry` (no
/// existing on-disk content). Extracted helper so the merge path
/// and the new-instance path share the field-emission logic.
fn build_instance_mapping(config: &InstanceYamlEntry) -> serde_yaml_ng::Mapping {
    let mut inst = serde_yaml_ng::Mapping::new();
    for (key, val) in [
        ("backend", &config.backend),
        ("working_directory", &config.working_directory),
        ("role", &config.role),
        ("instructions", &config.instructions),
        ("source_repo", &config.source_repo),
        ("repo", &config.repo),
        ("github_login", &config.github_login),
        ("model", &config.model),
        ("ready_pattern", &config.ready_pattern),
        ("command", &config.command),
    ] {
        if let Some(ref v) = val {
            inst.insert(key.into(), serde_yaml_ng::Value::String(v.clone()));
        }
    }
    if let Some(ref args) = config.args {
        let seq: Vec<serde_yaml_ng::Value> = args
            .iter()
            .map(|s| serde_yaml_ng::Value::String(s.clone()))
            .collect();
        inst.insert("args".into(), serde_yaml_ng::Value::Sequence(seq));
    }
    if let Some(ref env_map) = config.env {
        let mut env_yaml = serde_yaml_ng::Mapping::new();
        for (k, v) in env_map {
            env_yaml.insert(
                serde_yaml_ng::Value::String(k.clone()),
                serde_yaml_ng::Value::String(v.clone()),
            );
        }
        inst.insert("env".into(), serde_yaml_ng::Value::Mapping(env_yaml));
    }
    if let Some(worktree) = config.worktree {
        inst.insert("worktree".into(), serde_yaml_ng::Value::Bool(worktree));
    }
    inst
}

/// Add multiple instance entries to fleet.yaml in a single lock+write cycle.
///
/// Sprint 58 Wave 2 PR-2 (#525-9 / #16): this is now MERGE-aware
/// per the operator-resolved 3-tier baseline rules. New instance
/// names land as fresh mappings; existing instance names trigger
/// `merge_instance_into_existing` which respects DaemonManaged vs
/// OperatorHandEdit field classification + surfaces 真衝突 errors.
pub fn add_instances_to_yaml(home: &Path, entries: &[(&str, &InstanceYamlEntry)]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    mutate_fleet_yaml(home, "instances: {}\n", |doc| {
        if doc.get("instances").is_none() {
            doc["instances"] = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
        }
        let instances = doc
            .get_mut("instances")
            .and_then(|v| v.as_mapping_mut())
            .context("instances is not a mapping")?;

        let mut conflicts: Vec<FieldConflict> = Vec::new();

        for (name, config) in entries {
            let key = serde_yaml_ng::Value::String(name.to_string());
            // Sprint 58 Wave 2 PR-2: if the instance already exists,
            // merge into it per the 3-tier rules. Otherwise, build a
            // fresh mapping.
            if let Some(serde_yaml_ng::Value::Mapping(existing)) = instances.get_mut(&key) {
                if let Err(conflict) = merge_instance_into_existing(name, existing, config) {
                    conflicts.push(conflict);
                }
                tracing::info!(%name, "merged instance update into fleet.yaml");
            } else {
                let inst = build_instance_mapping(config);
                instances.insert(key, serde_yaml_ng::Value::Mapping(inst));
                tracing::info!(%name, "added new instance to fleet.yaml");
            }
        }

        if !conflicts.is_empty() {
            // Surface 真衝突 — operator must hand-resolve before
            // daemon's view lands. Format the diff as multi-line
            // for operator readability.
            let mut diff_lines: Vec<String> = Vec::with_capacity(conflicts.len());
            for c in &conflicts {
                diff_lines.push(format!("  - {c}"));
            }
            return Err(anyhow::anyhow!(
                "fleet.yaml merge conflict ({} field(s)):\n{}",
                conflicts.len(),
                diff_lines.join("\n")
            ));
        }
        Ok(())
    })
}

/// Remove an instance entry from fleet.yaml. Uses file lock + atomic write.
pub fn remove_instance_from_yaml(home: &Path, name: &str) -> Result<()> {
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) {
            instances.remove(serde_yaml_ng::Value::String(name.to_string()));
        }
        tracing::info!(%name, "removed instance from fleet.yaml");
        Ok(())
    })
}

/// Remove multiple instances from fleet.yaml in a single atomic write.
pub fn remove_instances_from_yaml(home: &Path, names: &[String]) -> Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) {
            for name in names {
                instances.remove(serde_yaml_ng::Value::String(name.clone()));
            }
        }
        Ok(())
    })
}

/// Atomically rewrite `channel.group_id` after a Telegram supergroup
/// migration. Mirrors [`update_instance_field`] for the top-level
/// `channel:` block; uses the same file lock + atomic write path so a
/// concurrent fleet read never observes a torn write.
///
/// The mutator is a no-op (Ok(())) when `channel:` is absent or its
/// `type` is not `telegram` — the migration error handler is the only
/// caller and it only fires on the telegram send path, so the no-op
/// branches are defensive belt-and-suspenders rather than intended
/// flow.
pub fn update_channel_telegram_group_id(home: &Path, new_group_id: i64) -> Result<()> {
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(channel) = doc.get_mut("channel").and_then(|v| v.as_mapping_mut()) {
            let is_telegram = channel
                .get(serde_yaml_ng::Value::String("type".into()))
                .and_then(|v| v.as_str())
                == Some("telegram");
            if is_telegram {
                channel.insert(
                    serde_yaml_ng::Value::String("group_id".into()),
                    serde_yaml_ng::Value::Number(new_group_id.into()),
                );
                tracing::info!(new_group_id, "fleet.yaml channel.group_id rewritten");
            }
        }
        Ok(())
    })
}

/// Update a specific field of an instance in fleet.yaml. Uses file lock + atomic write.
pub fn update_instance_field(
    home: &Path,
    name: &str,
    field: &str,
    value: serde_yaml_ng::Value,
) -> Result<()> {
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) {
            let key = serde_yaml_ng::Value::String(name.to_string());
            if let Some(inst) = instances.get_mut(&key).and_then(|v| v.as_mapping_mut()) {
                inst.insert(serde_yaml_ng::Value::String(field.to_string()), value);
            }
        }
        Ok(())
    })
}

/// Sprint 54 fleet-yaml teams unification: serialize a `TeamConfig` to a
/// `serde_yaml_ng::Mapping` for round-trip-safe insertion into the
/// `teams:` block. Mirrors `add_instance_to_yaml`'s manual-mapping
/// approach (avoids `to_value` re-roundtrip churn that could shift key
/// ordering between writes).
fn team_config_to_mapping(config: &TeamConfig) -> serde_yaml_ng::Mapping {
    let mut team = serde_yaml_ng::Mapping::new();
    let members_seq: Vec<serde_yaml_ng::Value> = config
        .members
        .iter()
        .map(|m| serde_yaml_ng::Value::String(m.clone()))
        .collect();
    team.insert(
        "members".into(),
        serde_yaml_ng::Value::Sequence(members_seq),
    );
    if let Some(ref orch) = config.orchestrator {
        team.insert(
            "orchestrator".into(),
            serde_yaml_ng::Value::String(orch.clone()),
        );
    }
    if let Some(ref desc) = config.description {
        team.insert(
            "description".into(),
            serde_yaml_ng::Value::String(desc.clone()),
        );
    }
    if let Some(ref ts) = config.created_at {
        team.insert(
            "created_at".into(),
            serde_yaml_ng::Value::String(ts.clone()),
        );
    }
    if let Some(ref sr) = config.source_repo {
        team.insert(
            "source_repo".into(),
            serde_yaml_ng::Value::String(sr.display().to_string()),
        );
    }
    team
}

/// Add a new team entry to fleet.yaml `teams:` block. Idempotent on
/// caller side: returns `Ok(false)` if a team with this name already
/// exists (so callers can surface the duplicate error to the user
/// without losing the existing entry); returns `Ok(true)` on insert.
/// Uses file lock + atomic write — same precedent as `add_instance_to_yaml`.
pub fn add_team_to_yaml(home: &Path, name: &str, config: &TeamConfig) -> Result<bool> {
    let mut inserted = false;
    mutate_fleet_yaml(home, "teams: {}\n", |doc| {
        if doc.get("teams").is_none() {
            doc["teams"] = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
        }
        let teams = doc
            .get_mut("teams")
            .and_then(|v| v.as_mapping_mut())
            .context("teams is not a mapping")?;
        let key = serde_yaml_ng::Value::String(name.to_string());
        if teams.contains_key(&key) {
            return Ok(());
        }
        teams.insert(
            key,
            serde_yaml_ng::Value::Mapping(team_config_to_mapping(config)),
        );
        inserted = true;
        tracing::info!(%name, "added team to fleet.yaml");
        Ok(())
    })?;
    Ok(inserted)
}

/// Remove a team entry from fleet.yaml. Returns whether a team was
/// actually removed (false when no such team existed). Uses file lock
/// + atomic write — same precedent as `remove_instance_from_yaml`.
pub fn remove_team_from_yaml(home: &Path, name: &str) -> Result<bool> {
    let mut removed = false;
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(teams) = doc.get_mut("teams").and_then(|v| v.as_mapping_mut()) {
            if teams
                .remove(serde_yaml_ng::Value::String(name.to_string()))
                .is_some()
            {
                removed = true;
                tracing::info!(%name, "removed team from fleet.yaml");
            }
        }
        Ok(())
    })?;
    Ok(removed)
}

/// Replace an existing team's full config in fleet.yaml. Returns
/// whether the team existed (false when no-op).
///
/// Used by `teams::update` + `teams::remove_member_from_all` after
/// building the desired post-mutation state in memory — single
/// round-trip, no field-level micro-mutation churn.
pub fn update_team_in_yaml(home: &Path, name: &str, config: &TeamConfig) -> Result<bool> {
    let mut existed = false;
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(teams) = doc.get_mut("teams").and_then(|v| v.as_mapping_mut()) {
            let key = serde_yaml_ng::Value::String(name.to_string());
            if teams.contains_key(&key) {
                teams.insert(
                    key,
                    serde_yaml_ng::Value::Mapping(team_config_to_mapping(config)),
                );
                existed = true;
            }
        }
        Ok(())
    })?;
    Ok(existed)
}

/// Sprint 54 fleet-yaml teams unification: one-shot migration from the
/// legacy `teams.json` runtime store into fleet.yaml's `teams:` block.
/// Idempotent — re-running is a safe no-op once `teams.json.migrated`
/// exists. Behavior:
///
/// - `teams.json` absent → no-op (already migrated, or fresh install)
/// - `teams.json` present → load it, merge each team into fleet.yaml
///   (skip teams already present in fleet.yaml — operator hand-edits win
///   over runtime store), rename `teams.json` → `teams.json.migrated`
///   (one-cycle safety, not deletion, per decision body).
///
/// Called from daemon startup before any team-touching path runs, so
/// post-migration `teams::list` / `find_team_for` etc. read the unified
/// fleet.yaml view.
pub fn migrate_teams_json_to_yaml(home: &Path) -> Result<()> {
    let teams_json = home.join("teams.json");
    if !teams_json.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(&teams_json)
        .with_context(|| format!("read teams.json: {}", teams_json.display()))?;
    // Defensive parse: empty / malformed file → log + leave file in place,
    // operator inspects manually rather than losing data.
    #[derive(Deserialize)]
    struct LegacyTeamStore {
        #[serde(default)]
        teams: Vec<LegacyTeam>,
    }
    #[derive(Deserialize)]
    struct LegacyTeam {
        name: String,
        #[serde(default)]
        members: Vec<String>,
        #[serde(default)]
        orchestrator: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        created_at: Option<String>,
    }
    let store: LegacyTeamStore = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, path = %teams_json.display(),
                "teams.json migration: parse failed, leaving file in place");
            return Ok(());
        }
    };
    if store.teams.is_empty() {
        // Empty store — still rename so the no-op-on-rerun branch
        // catches future startups instantly.
        let migrated = home.join("teams.json.migrated");
        std::fs::rename(&teams_json, &migrated)
            .with_context(|| format!("rename {} → {}", teams_json.display(), migrated.display()))?;
        tracing::info!("teams.json migration: empty store, renamed to .migrated");
        return Ok(());
    }
    for team in &store.teams {
        let cfg = TeamConfig {
            members: team.members.clone(),
            orchestrator: team.orchestrator.clone(),
            description: team.description.clone(),
            created_at: team.created_at.clone(),
            // Legacy `teams.json` schema (pre-Sprint-54) has no
            // `source_repo` field — migration physically cannot carry
            // over what doesn't exist. Hard-coded None is the only
            // option; downstream operator UX (#781 Piece 1a-i / 1a-ii
            // warn below + Bug A1 Team projection surfacing the field)
            // makes the resulting `None` state visible so the operator
            // knows to run `team update source_repo=...` instead of
            // silently falling to Tier 4 stub at dispatch time.
            source_repo: None,
        };
        // add_team_to_yaml is no-op when team already in fleet.yaml —
        // operator hand-edits win, runtime store loses on conflict.
        match add_team_to_yaml(home, &team.name, &cfg) {
            Ok(true) => {
                tracing::info!(name = %team.name, "migrated team to fleet.yaml");
                // #781 Piece 1a-i — operator-facing warn so the
                // legacy-migration source_repo=None state surfaces in
                // daemon logs at startup time, not at first failed
                // dispatch.
                tracing::warn!(
                    name = %team.name,
                    "migrated team from legacy teams.json without source_repo — \
                     set via `team(action=update, name={}, source_repo=...)` or \
                     daemon will fall to working_directory/stub Tier 3/4 in dispatch_auto_bind_lease",
                    team.name
                );
                // #781 Piece 1a-ii — event_log persists the warning
                // for post-mortem visibility; tracing logs rotate /
                // tail-only by default and operators may miss them.
                crate::event_log::log(
                    home,
                    "team_migration_missing_source_repo",
                    &team.name,
                    "legacy teams.json schema had no source_repo; \
                     set via team(action=update) to avoid Tier 4 stub fallback",
                );
            }
            Ok(false) => tracing::info!(name = %team.name,
                "team already in fleet.yaml, skipping migration entry"),
            Err(e) => {
                tracing::warn!(name = %team.name, error = %e,
                    "team migration failed, leaving teams.json in place");
                return Err(e);
            }
        }
    }
    let migrated = home.join("teams.json.migrated");
    std::fs::rename(&teams_json, &migrated)
        .with_context(|| format!("rename {} → {}", teams_json.display(), migrated.display()))?;
    tracing::info!(
        count = store.teams.len(),
        "teams.json migration complete, renamed to .migrated"
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    /// Shared mutex for tests that mutate process-global env vars.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static G: std::sync::Mutex<()> = std::sync::Mutex::new(());
        G.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write_fleet(dir: &Path, yaml: &str) -> PathBuf {
        fs::create_dir_all(dir).ok();
        let path = dir.join("fleet.yaml");
        fs::write(&path, yaml).expect("write fleet.yaml");
        path
    }

    #[test]
    fn test_preset_args_not_applied_to_different_command() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-test-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
defaults:
  backend: claude
instances:
  test:
    command: /bin/bash
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("test").expect("resolve");

        assert_eq!(resolved.backend_command, "/bin/bash");
        // Preset args (--dangerously-skip-permissions) should NOT be applied
        assert!(
            resolved.args.is_empty(),
            "args should be empty for non-preset command, got: {:?}",
            resolved.args
        );
        // Submit key should be default \r, not preset's
        assert_eq!(resolved.submit_key, "\r");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_resolved_args_exclude_preset() {
        // resolve_instance returns user-only args; preset args are injected
        // by agent::spawn_agent based on SpawnMode.
        let dir = std::env::temp_dir().join(format!("agend-fleet-test2-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
defaults:
  backend: claude
instances:
  test:
    command: claude
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("test").expect("resolve");

        assert_eq!(resolved.backend_command, "claude");
        assert!(
            resolved.args.is_empty(),
            "preset args must not appear in resolved.args, got: {:?}",
            resolved.args
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_env_merge_order() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-test3-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
defaults:
  env:
    KEY1: default_val
    KEY2: default_val
instances:
  test:
    command: /bin/bash
    env:
      KEY2: instance_val
      KEY3: instance_only
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("test").expect("resolve");

        assert_eq!(
            resolved.env.get("KEY1").map(|s| s.as_str()),
            Some("default_val")
        );
        assert_eq!(
            resolved.env.get("KEY2").map(|s| s.as_str()),
            Some("instance_val")
        ); // instance overrides
        assert_eq!(
            resolved.env.get("KEY3").map(|s| s.as_str()),
            Some("instance_only")
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_add_instance_to_yaml() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-add-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  existing:
    command: /bin/bash
"#,
        );
        let entry = InstanceYamlEntry {
            backend: Some("claude".to_string()),
            working_directory: Some("/tmp/work".to_string()),
            role: Some("developer".to_string()),
            instructions: Some("./instructions/dev.md".to_string()),
            source_repo: None,
            repo: None,
            github_login: None,
            args: None,
            model: None,
            env: None,
            ready_pattern: None,
            command: None,
            worktree: None,
        };
        add_instance_to_yaml(&dir, "new-agent", &entry).expect("add");
        let config = FleetConfig::load(&path).expect("load after add");
        assert!(config.instances.contains_key("new-agent"));
        let inst = &config.instances["new-agent"];
        assert_eq!(inst.backend, Some(crate::backend::Backend::ClaudeCode));
        assert_eq!(inst.working_directory.as_deref(), Some("/tmp/work"));
        assert_eq!(inst.role.as_deref(), Some("developer"));
        assert_eq!(inst.instructions.as_deref(), Some("./instructions/dev.md"));
        // existing instance should still be there
        assert!(config.instances.contains_key("existing"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_remove_instance_from_yaml() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-rm-{}", std::process::id()));
        write_fleet(
            &dir,
            r#"
instances:
  keep:
    command: /bin/bash
  remove-me:
    command: /bin/bash
"#,
        );
        remove_instance_from_yaml(&dir, "remove-me").expect("remove");
        let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load after remove");
        assert!(config.instances.contains_key("keep"));
        assert!(!config.instances.contains_key("remove-me"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_add_instance_creates_fleet_yaml() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-create-{}", std::process::id()));
        fs::create_dir_all(&dir).ok();
        // No fleet.yaml exists yet
        let entry = InstanceYamlEntry {
            backend: Some("claude".to_string()),
            working_directory: None,
            role: None,
            instructions: None,
            source_repo: None,
            repo: None,
            github_login: None,
            args: None,
            model: None,
            env: None,
            ready_pattern: None,
            command: None,
            worktree: None,
        };
        add_instance_to_yaml(&dir, "first", &entry).expect("add to new");
        let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        assert!(config.instances.contains_key("first"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_update_instance_field() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-upd-{}", std::process::id()));
        write_fleet(
            &dir,
            r#"
instances:
  agent1:
    command: /bin/bash
"#,
        );
        update_instance_field(
            &dir,
            "agent1",
            "topic_id",
            serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(42)),
        )
        .expect("update field");
        let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        assert_eq!(config.instances["agent1"].topic_id, Some(42));

        fs::remove_dir_all(&dir).ok();
    }

    // ─── Sprint 56 Track A — supergroup migration self-heal ────────────

    fn channel_yaml(group_id: i64) -> String {
        format!(
            r#"channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: {group_id}
  mode: topic
  user_allowlist:
    - 42
instances:
  agent1:
    command: /bin/bash
"#
        )
    }

    fn read_channel_group_id(home: &Path) -> Option<i64> {
        let text = std::fs::read_to_string(fleet_yaml_path(home)).ok()?;
        let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).ok()?;
        doc.get("channel")
            .and_then(|c| c.get("group_id"))
            .and_then(|v| v.as_i64())
    }

    #[test]
    fn update_channel_telegram_group_id_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-migrate-{}-{}",
            std::process::id(),
            line!()
        ));
        write_fleet(&dir, &channel_yaml(-100111));
        let new_id = -1009999999999_i64;

        update_channel_telegram_group_id(&dir, new_id).expect("update channel.group_id");

        assert_eq!(read_channel_group_id(&dir), Some(new_id));
        // Sibling fields and instances must survive the rewrite.
        let text = std::fs::read_to_string(dir.join("fleet.yaml")).unwrap();
        assert!(text.contains("bot_token_env: AGEND_BOT_TOKEN"));
        assert!(text.contains("mode: topic"));
        assert!(text.contains("user_allowlist"));
        assert!(text.contains("agent1"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_channel_telegram_group_id_idempotent_same_value() {
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-migrate-idem-{}-{}",
            std::process::id(),
            line!()
        ));
        let new_id = -1008888888888_i64;
        write_fleet(&dir, &channel_yaml(new_id));

        update_channel_telegram_group_id(&dir, new_id).expect("idempotent rewrite");
        update_channel_telegram_group_id(&dir, new_id).expect("idempotent twice");

        assert_eq!(read_channel_group_id(&dir), Some(new_id));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_channel_telegram_group_id_noop_on_non_telegram_channel() {
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-migrate-discord-{}-{}",
            std::process::id(),
            line!()
        ));
        write_fleet(
            &dir,
            r#"channel:
  type: discord
  bot_token_env: AGEND_DISCORD_BOT_TOKEN
  guild_id: 12345
"#,
        );

        update_channel_telegram_group_id(&dir, -100777).expect("noop on discord");

        // No `group_id` on a discord channel — must not have been
        // injected by the telegram-only helper.
        let text = std::fs::read_to_string(dir.join("fleet.yaml")).unwrap();
        assert!(
            !text.contains("group_id"),
            "telegram helper must not mutate discord channel: {text}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_channel_telegram_group_id_noop_when_no_channel_block() {
        // Self-heal helper is no-op-friendly: callers don't have to
        // pre-check, the disk write simply does nothing if there's no
        // channel block to mutate.
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-migrate-no-channel-{}-{}",
            std::process::id(),
            line!()
        ));
        write_fleet(
            &dir,
            r#"instances:
  agent1:
    command: /bin/bash
"#,
        );

        update_channel_telegram_group_id(&dir, -100777).expect("noop when no channel");
        // fleet.yaml must still parse and not have a synthesized channel.
        let text = std::fs::read_to_string(dir.join("fleet.yaml")).unwrap();
        assert!(!text.contains("channel:"), "must not synthesize channel");
        fs::remove_dir_all(&dir).ok();
    }

    // ─── Sprint 54 fleet-yaml teams unification ─────────────────────────

    fn tmp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-team-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn add_team_to_yaml_creates_entry_and_returns_true() {
        let dir = tmp_dir("add_team");
        let cfg = TeamConfig {
            members: vec!["alice".into(), "bob".into()],
            orchestrator: Some("alice".into()),
            description: Some("dev squad".into()),
            created_at: Some("2026-05-07T00:00:00Z".into()),
            source_repo: None,
        };
        let inserted = add_team_to_yaml(&dir, "devs", &cfg).expect("add");
        assert!(inserted);
        let loaded = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        let team = loaded.teams.get("devs").expect("team present");
        assert_eq!(team.members, vec!["alice", "bob"]);
        assert_eq!(team.orchestrator.as_deref(), Some("alice"));
        assert_eq!(team.created_at.as_deref(), Some("2026-05-07T00:00:00Z"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_team_to_yaml_duplicate_returns_false_without_overwrite() {
        let dir = tmp_dir("dup");
        let cfg1 = TeamConfig {
            members: vec!["alice".into()],
            orchestrator: Some("alice".into()),
            description: None,
            created_at: Some("2026-05-07T00:00:00Z".into()),
            source_repo: None,
        };
        assert!(add_team_to_yaml(&dir, "devs", &cfg1).expect("first"));
        let cfg2 = TeamConfig {
            members: vec!["bob".into()],
            orchestrator: Some("bob".into()),
            description: None,
            created_at: Some("2026-05-08T00:00:00Z".into()),
            source_repo: None,
        };
        let inserted = add_team_to_yaml(&dir, "devs", &cfg2).expect("dup");
        assert!(!inserted, "duplicate must report false (no insert)");
        let loaded = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        let team = loaded.teams.get("devs").expect("team present");
        assert_eq!(
            team.members,
            vec!["alice"],
            "first writer wins — original entry preserved"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn remove_team_from_yaml_returns_whether_existed() {
        let dir = tmp_dir("rm_team");
        let cfg = TeamConfig {
            members: vec!["alice".into()],
            orchestrator: Some("alice".into()),
            description: None,
            created_at: None,
            source_repo: None,
        };
        add_team_to_yaml(&dir, "devs", &cfg).expect("add");
        assert!(remove_team_from_yaml(&dir, "devs").expect("rm"));
        let loaded = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        assert!(loaded.teams.is_empty());
        // Second remove → false
        assert!(!remove_team_from_yaml(&dir, "devs").expect("rm-again"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_team_in_yaml_replaces_full_config() {
        let dir = tmp_dir("upd_team");
        let cfg = TeamConfig {
            members: vec!["alice".into(), "bob".into()],
            orchestrator: Some("alice".into()),
            description: None,
            created_at: Some("2026-05-07T00:00:00Z".into()),
            source_repo: None,
        };
        add_team_to_yaml(&dir, "devs", &cfg).expect("add");
        // Drop bob, reassign orch to alice (no-op), add carol.
        let new_cfg = TeamConfig {
            members: vec!["alice".into(), "carol".into()],
            orchestrator: Some("alice".into()),
            description: Some("post-update".into()),
            created_at: cfg.created_at.clone(),
            source_repo: None,
        };
        assert!(update_team_in_yaml(&dir, "devs", &new_cfg).expect("upd"));
        let loaded = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        let team = loaded.teams.get("devs").expect("team present");
        assert_eq!(team.members, vec!["alice", "carol"]);
        assert_eq!(team.description.as_deref(), Some("post-update"));
        // Update on missing team → false (no insert)
        let new_cfg2 = TeamConfig {
            members: vec!["x".into()],
            orchestrator: None,
            description: None,
            created_at: None,
            source_repo: None,
        };
        assert!(!update_team_in_yaml(&dir, "nonexistent", &new_cfg2).expect("upd-miss"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrate_teams_json_to_yaml_moves_teams_and_renames_marker() {
        let dir = tmp_dir("migrate_present");
        // Seed legacy teams.json
        let legacy = r#"{"schema_version":1,"teams":[
            {"name":"devs","members":["alice","bob"],"orchestrator":"alice","description":"the devs","created_at":"2026-05-07T00:00:00Z"},
            {"name":"ops","members":["lead"],"orchestrator":"lead","created_at":"2026-05-07T01:00:00Z"}
        ]}"#;
        fs::write(dir.join("teams.json"), legacy).unwrap();
        migrate_teams_json_to_yaml(&dir).expect("migrate");
        // teams.json renamed
        assert!(
            !dir.join("teams.json").exists(),
            "teams.json must be renamed"
        );
        assert!(dir.join("teams.json.migrated").exists(), "marker missing");
        // fleet.yaml has both teams
        let loaded = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        assert_eq!(loaded.teams.len(), 2);
        let devs = loaded.teams.get("devs").expect("devs");
        assert_eq!(devs.members, vec!["alice", "bob"]);
        assert_eq!(devs.orchestrator.as_deref(), Some("alice"));
        assert_eq!(devs.description.as_deref(), Some("the devs"));
        assert_eq!(devs.created_at.as_deref(), Some("2026-05-07T00:00:00Z"));
        let ops = loaded.teams.get("ops").expect("ops");
        assert_eq!(ops.orchestrator.as_deref(), Some("lead"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrate_teams_json_to_yaml_idempotent_on_rerun() {
        let dir = tmp_dir("migrate_idempotent");
        fs::write(
            dir.join("teams.json"),
            r#"{"schema_version":1,"teams":[{"name":"devs","members":["a"],"orchestrator":"a","created_at":"x"}]}"#,
        )
        .unwrap();
        migrate_teams_json_to_yaml(&dir).expect("first");
        // Second run with no teams.json present → no-op, no error.
        migrate_teams_json_to_yaml(&dir).expect("second");
        let loaded = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        assert_eq!(loaded.teams.len(), 1, "no duplicate after re-run");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrate_teams_json_to_yaml_absent_is_noop() {
        let dir = tmp_dir("migrate_absent");
        // No teams.json — must not error, must not create fleet.yaml.
        migrate_teams_json_to_yaml(&dir).expect("absent");
        assert!(!dir.join("fleet.yaml").exists());
        assert!(!dir.join("teams.json.migrated").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrate_teams_json_to_yaml_preserves_existing_fleet_team() {
        // Operator hand-edited fleet.yaml `teams:` entry must win over
        // the same-name entry in legacy teams.json — operator wins on
        // conflict, runtime store loses.
        let dir = tmp_dir("migrate_conflict");
        fs::write(
            dir.join("fleet.yaml"),
            "teams:\n  devs:\n    members: [hand-edited]\n    orchestrator: hand-edited\n",
        )
        .unwrap();
        fs::write(
            dir.join("teams.json"),
            r#"{"schema_version":1,"teams":[{"name":"devs","members":["alice","bob"],"orchestrator":"alice","created_at":"x"}]}"#,
        )
        .unwrap();
        migrate_teams_json_to_yaml(&dir).expect("migrate");
        let loaded = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        let team = loaded.teams.get("devs").expect("present");
        assert_eq!(
            team.members,
            vec!["hand-edited"],
            "operator hand-edit must survive migration: got {:?}",
            team.members
        );
        // teams.json still gets renamed even on conflict (we treat the
        // store as "consumed" once visited; .migrated marker stops
        // future re-attempts from clobbering).
        assert!(dir.join("teams.json.migrated").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_channel_config_telegram_parsing() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-chan-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
channel:
  type: telegram
  bot_token_env: MY_BOT_TOKEN
  group_id: -100123456
  mode: topic
instances:
  test:
    command: /bin/bash
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        match config.channel {
            Some(ChannelConfig::Telegram {
                ref bot_token_env,
                group_id,
                ref mode,
                ..
            }) => {
                assert_eq!(bot_token_env, "MY_BOT_TOKEN");
                assert_eq!(group_id, -100123456);
                assert_eq!(mode, "topic");
            }
            None => panic!("channel should be Some"),
            Some(crate::fleet::ChannelConfig::Discord { .. }) => panic!("unexpected discord"),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_channels_plural_single_entry_collapses_to_singular() {
        // `channels:` (plural) with one entry normalizes into `channel:`
        // so downstream readers that only know the singular field keep
        // working unchanged.
        let dir =
            std::env::temp_dir().join(format!("agend-fleet-chan-plural-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
channels:
  tg-main:
    type: telegram
    bot_token_env: MY_BOT_TOKEN
    group_id: -100999
    mode: topic
instances: {}
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        match config.channel {
            Some(ChannelConfig::Telegram {
                ref bot_token_env,
                group_id,
                ..
            }) => {
                assert_eq!(bot_token_env, "MY_BOT_TOKEN");
                assert_eq!(group_id, -100999);
            }
            None => panic!("plural channels: should populate singular channel field"),
            Some(crate::fleet::ChannelConfig::Discord { .. }) => panic!("unexpected discord"),
        }
        // Plural is still preserved on the struct for later consumers.
        assert!(config.channels.is_some());
        assert_eq!(config.channels.as_ref().unwrap().len(), 1);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_channels_plural_multi_entry_picks_first_by_name() {
        // Multi-channel routing is not yet wired; normalize must pick a
        // deterministic entry (first by sorted key) so runtime behavior
        // does not depend on HashMap iteration order.
        let dir =
            std::env::temp_dir().join(format!("agend-fleet-chan-multi-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
channels:
  zeta:
    type: telegram
    bot_token_env: ZETA_TOKEN
    group_id: -3
  alpha:
    type: telegram
    bot_token_env: ALPHA_TOKEN
    group_id: -1
  mid:
    type: telegram
    bot_token_env: MID_TOKEN
    group_id: -2
instances: {}
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        match config.channel {
            Some(ChannelConfig::Telegram {
                ref bot_token_env, ..
            }) => {
                assert_eq!(
                    bot_token_env, "ALPHA_TOKEN",
                    "must pick first entry by sorted name"
                );
            }
            None => panic!("channel should be populated"),
            Some(crate::fleet::ChannelConfig::Discord { .. }) => panic!("unexpected discord"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_channel_singular_wins_when_both_set() {
        // Byte-identical runtime for inputs that already wrote `channel:`:
        // even if `channels:` is also present, the singular form wins and
        // `normalize()` leaves it alone.
        let dir =
            std::env::temp_dir().join(format!("agend-fleet-chan-both-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
channel:
  type: telegram
  bot_token_env: SINGULAR_TOKEN
  group_id: -111
channels:
  plural-entry:
    type: telegram
    bot_token_env: PLURAL_TOKEN
    group_id: -222
instances: {}
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        match config.channel {
            Some(ChannelConfig::Telegram {
                ref bot_token_env, ..
            }) => assert_eq!(bot_token_env, "SINGULAR_TOKEN"),
            None => panic!("singular channel field must be preserved"),
            Some(crate::fleet::ChannelConfig::Discord { .. }) => panic!("unexpected discord"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_channel_absent_when_neither_form_set() {
        // Zero-config case: no channel wiring, no warnings, no panics.
        let dir =
            std::env::temp_dir().join(format!("agend-fleet-chan-none-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  a:
    backend: claude
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        assert!(config.channel.is_none());
        assert!(config.channels.is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_channel_config_default_mode() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-defmode-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
channel:
  type: telegram
  bot_token_env: TOKEN
  group_id: -999
instances: {}
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        match config.channel {
            Some(ChannelConfig::Telegram { ref mode, .. }) => {
                assert_eq!(mode, "topic", "default mode should be 'topic'");
            }
            None => panic!("channel should be Some"),
            Some(crate::fleet::ChannelConfig::Discord { .. }) => panic!("unexpected discord"),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_missing_defaults_still_works() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-nodef-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  agent1:
    command: /bin/bash
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        assert!(config.defaults.backend.is_none());
        assert!(config.defaults.command.is_none());
        assert!(config.defaults.model.is_none());
        let resolved = config.resolve_instance("agent1").expect("resolve");
        assert_eq!(resolved.backend_command, "/bin/bash");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_instance_names_returns_all() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-names-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  alpha:
    command: /bin/bash
  beta:
    command: /bin/sh
  gamma:
    command: /bin/zsh
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let mut names = config.instance_names();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_add_remove_instance_roundtrip() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-addrem-{}", std::process::id()));
        write_fleet(&dir, "instances: {}\n");

        let entry = InstanceYamlEntry {
            backend: Some("claude".to_string()),
            working_directory: None,
            role: Some("tester".to_string()),
            instructions: None,
            source_repo: None,
            repo: None,
            github_login: None,
            args: None,
            model: None,
            env: None,
            ready_pattern: None,
            command: None,
            worktree: None,
        };
        add_instance_to_yaml(&dir, "temp-agent", &entry).expect("add");

        let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        assert!(config.instances.contains_key("temp-agent"));

        remove_instance_from_yaml(&dir, "temp-agent").expect("remove");
        let config2 = FleetConfig::load(&dir.join("fleet.yaml")).expect("load after remove");
        assert!(!config2.instances.contains_key("temp-agent"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_working_directory_tilde_expansion() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-tilde-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  agent1:
    command: /bin/bash
    working_directory: "~/project"
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("agent1").expect("resolve");
        let wd = resolved
            .working_directory
            .expect("should have working_directory");
        // Should NOT start with ~
        assert!(
            !wd.to_string_lossy().starts_with('~'),
            "tilde should be expanded, got: {}",
            wd.display()
        );
        // Should end with the `project` component — compare via Path so the
        // separator flip on Windows (`\`) doesn't trip a plain string match.
        assert!(
            wd.ends_with("project"),
            "should end with project, got: {}",
            wd.display()
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_working_directory_absolute_unchanged() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-abs-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  agent1:
    command: /bin/bash
    working_directory: "/absolute/path"
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("agent1").expect("resolve");
        let wd = resolved.working_directory.expect("should have wd");
        assert_eq!(wd.to_string_lossy(), "/absolute/path");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_resolve_nonexistent_instance() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-noinst-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  agent1:
    command: /bin/bash
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        assert!(config.resolve_instance("nonexistent").is_none());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_teams_parsing() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-teams-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  a1:
    command: /bin/bash
  a2:
    command: /bin/bash
teams:
  dev:
    members:
      - a1
      - a2
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let team = config.teams.get("dev").expect("team exists");
        assert_eq!(team.members, vec!["a1", "a2"]);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_instance_env_includes_agend_name() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-envname-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  my-agent:
    command: /bin/bash
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("my-agent").expect("resolve");
        assert_eq!(
            resolved.env.get("AGEND_INSTANCE_NAME").map(|s| s.as_str()),
            Some("my-agent")
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_cols_rows_override() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-colrow-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
defaults:
  cols: 80
  rows: 24
instances:
  default-size:
    command: /bin/bash
  custom-size:
    command: /bin/bash
    cols: 200
    rows: 50
"#,
        );
        let config = FleetConfig::load(&path).expect("load");

        let def = config.resolve_instance("default-size").expect("resolve");
        assert_eq!(def.cols, Some(80));
        assert_eq!(def.rows, Some(24));

        let custom = config.resolve_instance("custom-size").expect("resolve");
        assert_eq!(custom.cols, Some(200));
        assert_eq!(custom.rows, Some(50));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_git_branch_override() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-test4-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  with_branch:
    command: /bin/bash
    git_branch: "custom/branch"
  without_branch:
    command: /bin/bash
"#,
        );
        let config = FleetConfig::load(&path).expect("load");

        let with = config.resolve_instance("with_branch").expect("resolve");
        assert_eq!(with.git_branch.as_deref(), Some("custom/branch"));

        let without = config.resolve_instance("without_branch").expect("resolve");
        assert!(without.git_branch.is_none());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_topic_id_parsed() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-topic-{}", std::process::id()));
        fs::create_dir_all(&dir).ok();
        let path = dir.join("fleet.yaml");
        fs::write(
            &path,
            r#"instances:
  alice:
    backend: claude
    topic_id: 229
  general:
    backend: claude
    topic_id: 1
"#,
        )
        .ok();
        let config = FleetConfig::load(&path).expect("load");
        assert_eq!(
            config.instances.get("alice").and_then(|i| i.topic_id),
            Some(229)
        );
        assert_eq!(
            config.instances.get("general").and_then(|i| i.topic_id),
            Some(1)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_topic_id_none_when_missing() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-notopic-{}", std::process::id()));
        fs::create_dir_all(&dir).ok();
        let path = dir.join("fleet.yaml");
        fs::write(
            &path,
            r#"instances:
  dev:
    backend: claude
"#,
        )
        .ok();
        let config = FleetConfig::load(&path).expect("load");
        assert_eq!(config.instances.get("dev").and_then(|i| i.topic_id), None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_remove_instance_preserves_other_topics() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-rmtopic-{}", std::process::id()));
        fs::create_dir_all(&dir).ok();
        let path = dir.join("fleet.yaml");
        fs::write(
            &path,
            r#"instances:
  alice:
    backend: claude
    topic_id: 229
  bob:
    backend: claude
    topic_id: 300
"#,
        )
        .ok();
        remove_instance_from_yaml(&dir, "alice").expect("remove");
        let config = FleetConfig::load(&path).expect("load");
        assert!(!config.instances.contains_key("alice"));
        assert_eq!(
            config.instances.get("bob").and_then(|i| i.topic_id),
            Some(300)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_default_working_directory() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-defwd-{}", std::process::id()));
        fs::create_dir_all(&dir).ok();
        let path = dir.join("fleet.yaml");
        fs::write(
            &path,
            r#"instances:
  alice:
    backend: claude
  bob:
    backend: claude
    working_directory: /tmp/custom
"#,
        )
        .ok();
        let config = FleetConfig::load(&path).expect("load");

        // alice: no working_directory → defaults to $AGEND_HOME/workspace/alice
        let alice = config.resolve_instance("alice").expect("alice");
        let wd = alice.working_directory.expect("wd");
        // Compare components (not strings) so `\` on Windows doesn't fail.
        assert!(
            wd.ends_with("workspace/alice"),
            "expected default workspace path, got: {}",
            wd.display()
        );

        // bob: explicit working_directory → used as-is
        let bob = config.resolve_instance("bob").expect("bob");
        assert_eq!(
            bob.working_directory.expect("wd"),
            std::path::PathBuf::from("/tmp/custom")
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_working_directory_always_some() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-wdsome-{}", std::process::id()));
        fs::create_dir_all(&dir).ok();
        let path = dir.join("fleet.yaml");
        fs::write(
            &path,
            r#"instances:
  minimal:
    backend: claude
"#,
        )
        .ok();
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("minimal").expect("resolve");
        assert!(
            resolved.working_directory.is_some(),
            "working_directory must always be Some after resolve"
        );
        fs::remove_dir_all(&dir).ok();
    }

    // ── Normalize: backend is derived from legacy `command:` at load ─────

    #[test]
    fn normalize_legacy_command_only_becomes_backend() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-norm1-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
defaults:
  command: /bin/bash
instances:
  worker:
    command: /opt/custom/tool
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        // Absolute paths preserve the literal — a later spawn uses them
        // verbatim. Only the bare names `shell|bash|zsh|sh` fold into Shell.
        assert_eq!(
            config.defaults.backend,
            Some(Backend::Raw("/bin/bash".to_string()))
        );
        assert_eq!(
            config
                .instances
                .get("worker")
                .and_then(|i| i.backend.clone()),
            Some(Backend::Raw("/opt/custom/tool".to_string()))
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn normalize_legacy_command_with_known_preset_name() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-norm2-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
defaults:
  command: claude
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        assert_eq!(config.defaults.backend, Some(Backend::ClaudeCode));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn normalize_explicit_backend_takes_precedence_over_command() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-norm3-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  worker:
    backend: claude
    command: /custom/claude-v2
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        // Explicit backend wins — command remains for resolve_instance to use as override.
        let inst = config.instances.get("worker").expect("worker");
        assert_eq!(inst.backend, Some(Backend::ClaudeCode));
        assert_eq!(inst.command.as_deref(), Some("/custom/claude-v2"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_new_shell_variant() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-norm4-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  bash_pane:
    backend: shell
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        assert_eq!(
            config
                .instances
                .get("bash_pane")
                .and_then(|i| i.backend.clone()),
            Some(Backend::Shell)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_new_raw_variant_as_bare_path() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-norm5-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  custom:
    backend: /opt/foo/bar
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        assert_eq!(
            config
                .instances
                .get("custom")
                .and_then(|i| i.backend.clone()),
            Some(Backend::Raw("/opt/foo/bar".to_string()))
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_backend_plus_command_override_preserves_backend_contract() {
        // `backend:` is the preset contract; `command:` is purely the spawn
        // path. resolve_instance returns user-only args (empty here); the
        // preset flags are injected at spawn time by agent::spawn_agent.
        let dir = std::env::temp_dir().join(format!("agend-fleet-override-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
instances:
  test:
    backend: claude
    command: /opt/claude-v2/my-claude
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("test").expect("resolve");
        assert_eq!(resolved.backend_command, "/opt/claude-v2/my-claude");
        assert!(
            resolved.args.is_empty(),
            "resolved.args must be user-only, got: {:?}",
            resolved.args
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn normalize_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-norm6-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
defaults:
  command: zsh
"#,
        );
        let mut config = FleetConfig::load(&path).expect("load");
        let before = config.defaults.backend.clone();
        config.normalize();
        // Running it again produces the same result.
        assert_eq!(config.defaults.backend, before);
        // Bare "zsh" (no leading slash) is the shell alias.
        assert_eq!(config.defaults.backend, Some(Backend::Shell));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn worktree_opt_out_parsed() {
        let yaml = "instances:\n  lead:\n    backend: claude\n    worktree: false\n  impl:\n    backend: claude\n";
        let config: FleetConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.instances["lead"].worktree, Some(false));
        assert_eq!(config.instances["impl"].worktree, None);
    }

    /// §3.5.10 canonical round-trip fixture: parse fleet.yaml via
    /// serde_yaml_ng → serialize → verify semantic equivalence +
    /// idempotent serialization. Uses Path B (canonical snapshot)
    /// because YAML serializers don't preserve comments/quote-style
    /// exactly, and HashMap iteration order is nondeterministic.
    ///
    /// Production-path-coupled: uses the real serde_yaml_ng import.
    #[test]
    fn serde_yaml_ng_canonical_round_trip() {
        // Single instance to avoid HashMap iteration order nondeterminism.
        let input = "defaults:\n  backend: claude\ninstances:\n  dev:\n    backend: kiro-cli\n    topic_id: 42\n";
        let config: FleetConfig = serde_yaml_ng::from_str(input).unwrap();
        let output = serde_yaml_ng::to_string(&config).unwrap();
        let reparsed: FleetConfig = serde_yaml_ng::from_str(&output).unwrap();

        // Semantic equivalence.
        assert_eq!(reparsed.instances.len(), 1);
        assert_eq!(reparsed.instances["dev"].topic_id, Some(42));
        assert_eq!(
            reparsed.instances["dev"].backend,
            Some(crate::backend::Backend::KiroCli)
        );

        // Idempotence: second serialize must match first.
        let output2 = serde_yaml_ng::to_string(&reparsed).unwrap();
        assert_eq!(output, output2, "serialization must be idempotent");

        // Adversarial: numeric as integer, strings unquoted.
        assert!(output.contains("42"), "topic_id must appear as integer");
        assert!(
            output.contains("kiro-cli"),
            "string values preserved: {output}"
        );
    }

    #[test]
    fn backfill_ids_opt_out_no_writeback() {
        // Local env guard for test isolation

        let _g = env_guard();
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-backfill-optout-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok();
        let yaml = "instances:\n  test-agent:\n    backend: claude\n";
        let path = dir.join("fleet.yaml");
        std::fs::write(&path, yaml).expect("write");
        std::env::set_var("AGEND_FLEET_NO_AUTO_MIGRATE", "1");
        let _ = FleetConfig::load(&path);
        std::env::remove_var("AGEND_FLEET_NO_AUTO_MIGRATE");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(
            !content.contains("id:"),
            "opt-out should prevent id writeback: {content}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backfill_ids_writes_when_opt_out_unset() {
        // Same guard as backfill_ids_opt_out_no_writeback

        let _g = env_guard();
        let dir =
            std::env::temp_dir().join(format!("agend-fleet-backfill-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let yaml = "instances:\n  test-agent:\n    backend: claude\n";
        let path = dir.join("fleet.yaml");
        std::fs::write(&path, yaml).expect("write");
        std::env::remove_var("AGEND_FLEET_NO_AUTO_MIGRATE");
        let _ = FleetConfig::load(&path);
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(
            content.contains("id:"),
            "writeback should add id field: {content}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ─── Sprint 54 P1-B Bug 2 fix: source_repo decouple tests ───────

    /// Backward-compat: fleet.yaml that predates the field deserializes
    /// cleanly. `source_repo` defaults to None — `dispatch_auto_bind_lease`
    /// will fall back to `working_directory`. Locks the
    /// `#[serde(default)]` contract.
    #[test]
    fn instance_config_deserializes_without_source_repo_field() {
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-srf-bc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let path = write_fleet(
            &dir,
            r#"
instances:
  legacy-agent:
    backend: claude
    working_directory: /tmp/legacy-agent
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let inst = config.instances.get("legacy-agent").expect("inst present");
        assert!(
            inst.source_repo.is_none(),
            "source_repo defaults to None when omitted: {inst:?}"
        );
        let resolved = config.resolve_instance("legacy-agent").expect("resolve");
        assert!(resolved.source_repo.is_none());
        assert_eq!(
            resolved
                .working_directory
                .as_deref()
                .map(|p| p.to_str().unwrap_or("")),
            Some("/tmp/legacy-agent"),
            "working_directory still resolves for backward-compat callers"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// New field round-trip: when fleet.yaml carries `source_repo`,
    /// the resolved struct surfaces it as a `PathBuf` distinct from
    /// `working_directory`. Locks the schema-decouple contract that
    /// dispatch_auto_bind_lease relies on.
    #[test]
    fn instance_config_resolves_source_repo_when_set() {
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-srf-set-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let path = write_fleet(
            &dir,
            r#"
instances:
  opted-in-agent:
    backend: claude
    working_directory: /tmp/opted-in-agent-state
    source_repo: /tmp/opted-in-agent-source
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("opted-in-agent").expect("resolve");
        assert_eq!(
            resolved
                .source_repo
                .as_deref()
                .map(|p| p.to_str().unwrap_or("")),
            Some("/tmp/opted-in-agent-source"),
            "source_repo resolves to the explicit fleet.yaml value"
        );
        assert_eq!(
            resolved
                .working_directory
                .as_deref()
                .map(|p| p.to_str().unwrap_or("")),
            Some("/tmp/opted-in-agent-state"),
            "working_directory remains the per-agent state-home dir, decoupled from source_repo"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// `~/` expansion applies to source_repo with the same treatment
    /// `working_directory` already gets. Locks parity so operator
    /// muscle-memory transfers cleanly between the two fields.
    #[test]
    fn instance_config_source_repo_tilde_expanded() {
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-srf-tilde-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let path = write_fleet(
            &dir,
            r#"
instances:
  tilde-agent:
    backend: claude
    source_repo: ~/op-clone
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("tilde-agent").expect("resolve");
        let source_repo = resolved.source_repo.expect("source_repo set");
        let expected_home = dirs::home_dir().expect("home dir");
        assert_eq!(
            source_repo,
            expected_home.join("op-clone"),
            "~ expansion must match working_directory's behaviour"
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Round-trip: writing an `InstanceYamlEntry` with `source_repo`
    /// set persists the field to disk in a form that re-loads
    /// identically. Locks the writer-reader symmetry — without this,
    /// operators editing fleet.yaml could see their `source_repo`
    /// silently disappear on the next daemon write.
    #[test]
    fn add_instance_to_yaml_round_trips_source_repo() {
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-srf-rt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(&dir).ok();
        let entry = InstanceYamlEntry {
            backend: Some("claude".to_string()),
            working_directory: Some("/tmp/rt-state".to_string()),
            role: Some("opted-in test agent".to_string()),
            instructions: None,
            source_repo: Some("/tmp/rt-source".to_string()),
            repo: None,
            github_login: None,
            args: None,
            model: None,
            env: None,
            ready_pattern: None,
            command: None,
            worktree: None,
        };
        add_instance_to_yaml(&dir, "rt-agent", &entry).expect("add");
        let content = std::fs::read_to_string(dir.join("fleet.yaml")).expect("read");
        assert!(
            content.contains("source_repo: /tmp/rt-source"),
            "source_repo must round-trip through the writer: {content}"
        );
        let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        let resolved = config.resolve_instance("rt-agent").expect("resolve");
        assert_eq!(
            resolved
                .source_repo
                .as_deref()
                .map(|p| p.to_str().unwrap_or("")),
            Some("/tmp/rt-source"),
        );
        fs::remove_dir_all(&dir).ok();
    }

    // ─────────────────────────────────────────────────────────────
    // Sprint 58 Wave 2 PR-2 (#525-9 / #16) — fleet.yaml merge logic
    // tests covering the operator-resolved 3-tier baseline rules.
    // ─────────────────────────────────────────────────────────────

    fn merge_tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-merge-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn read_yaml(home: &std::path::Path) -> serde_yaml_ng::Value {
        let content = std::fs::read_to_string(fleet_yaml_path(home)).expect("read");
        serde_yaml_ng::from_str(&content).expect("parse")
    }

    #[test]
    fn instance_field_class_categorizes_daemon_managed() {
        // Pin the categorization: daemon-managed identity / bookkeeping
        // fields must round-trip via DaemonManaged. A future addition
        // that drops one of these would silently break the merge
        // contract.
        for field in &["id", "topic_id", "git_branch", "source_repo"] {
            assert_eq!(
                instance_field_class(field),
                FieldClass::DaemonManaged,
                "field `{field}` must be DaemonManaged per operator-resolved baseline"
            );
        }
    }

    #[test]
    fn instance_field_class_categorizes_operator_hand_edit_default() {
        // Default to OperatorHandEdit for any unknown field —
        // operator's additions get preserved automatically.
        for field in &[
            "backend",
            "working_directory",
            "role",
            "instructions",
            "command",
            "args",
            "env",
            "model",
            "ready_pattern",
            "display_name",
            "github_login",
            "repo",
            "worktree",
            "totally_new_field_we_dont_know_about",
        ] {
            assert_eq!(
                instance_field_class(field),
                FieldClass::OperatorHandEdit,
                "field `{field}` must default to OperatorHandEdit"
            );
        }
    }

    #[test]
    fn merge_overwrites_daemon_managed_fields_when_changed_by_daemon() {
        // Lead spec — Rule 1: DaemonManaged + daemon supplies →
        // OVERWRITE existing.
        let home = merge_tmp_home("rule-1-overwrite");
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  dev:\n    backend: claude\n    source_repo: /old/path\n    role: \
             developer\n",
        )
        .unwrap();

        let entry = InstanceYamlEntry {
            backend: None,
            working_directory: None,
            role: None,
            source_repo: Some("/new/path".into()),
            ..Default::default()
        };
        add_instances_to_yaml(&home, &[("dev", &entry)]).unwrap();

        let v = read_yaml(&home);
        let dev = &v["instances"]["dev"];
        assert_eq!(
            dev["source_repo"], "/new/path",
            "DaemonManaged field MUST be overwritten"
        );
        assert_eq!(
            dev["role"], "developer",
            "OperatorHandEdit unchanged value preserved"
        );
        assert_eq!(dev["backend"], "claude");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merge_deep_merges_operator_hand_edit_fields_preserves_operator_additions() {
        // Lead spec — Rule 2: operator-added fields daemon doesn't
        // know about must survive the merge unchanged.
        let home = merge_tmp_home("rule-2-deep-merge");
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  dev:\n    backend: claude\n    role: developer\n    custom_operator_field: \
             my-special-value\n    display_name: Friendly Dev\n",
        )
        .unwrap();

        // Daemon writes a partial update that doesn't touch operator's
        // custom_operator_field or display_name.
        let entry = InstanceYamlEntry {
            backend: Some("claude".into()), // same as existing — no-op
            ..Default::default()
        };
        add_instances_to_yaml(&home, &[("dev", &entry)]).unwrap();

        let v = read_yaml(&home);
        let dev = &v["instances"]["dev"];
        assert_eq!(dev["custom_operator_field"], "my-special-value");
        assert_eq!(dev["display_name"], "Friendly Dev");
        assert_eq!(dev["role"], "developer");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merge_errors_on_incompatible_concurrent_changes_with_diff_output() {
        // Lead spec — Rule 3: 真衝突 same operator-hand-edit field
        // both sides changed incompatibly → error with diff.
        let home = merge_tmp_home("rule-3-conflict");
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  dev:\n    backend: claude\n    role: senior-dev\n",
        )
        .unwrap();

        // Daemon proposes a different role — incompatible with operator's.
        let entry = InstanceYamlEntry {
            role: Some("lead".into()),
            ..Default::default()
        };
        let result = add_instances_to_yaml(&home, &[("dev", &entry)]);
        assert!(
            result.is_err(),
            "incompatible operator-edit field MUST error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("merge conflict") || err.contains("conflict"),
            "error must mention merge conflict; got: {err}"
        );
        assert!(err.contains("dev"), "error must name the affected instance");
        assert!(
            err.contains("role"),
            "error must name the conflicting field"
        );
        assert!(
            err.contains("senior-dev") && err.contains("lead"),
            "error must show diff (operator value + daemon value); got: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merge_idempotent_when_no_changes() {
        // Daemon writes the same values that are already on disk —
        // no error, no spurious churn.
        let home = merge_tmp_home("idempotent");
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  dev:\n    backend: claude\n    role: developer\n    source_repo: /x\n",
        )
        .unwrap();
        let entry = InstanceYamlEntry {
            backend: Some("claude".into()),
            role: Some("developer".into()),
            source_repo: Some("/x".into()),
            ..Default::default()
        };
        let result = add_instances_to_yaml(&home, &[("dev", &entry)]);
        assert!(
            result.is_ok(),
            "idempotent merge MUST succeed: {:?}",
            result
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merge_handles_first_time_against_operator_hand_edits() {
        // Migration path: operator hand-edited fleet.yaml BEFORE
        // daemon ever wrote it. Daemon's first write must NOT clobber
        // operator's hand-edits — only fill in fields daemon
        // explicitly supplies.
        let home = merge_tmp_home("first-time-migration");
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  reviewer:\n    backend: claude\n    role: code-reviewer\n    \
             instructions: ./reviewer-instructions.md\n",
        )
        .unwrap();

        // Daemon's first write only sets source_repo (DaemonManaged).
        let entry = InstanceYamlEntry {
            source_repo: Some("/Users/dev/.agend".into()),
            ..Default::default()
        };
        add_instances_to_yaml(&home, &[("reviewer", &entry)]).unwrap();

        let v = read_yaml(&home);
        let reviewer = &v["instances"]["reviewer"];
        // Operator hand-edits preserved.
        assert_eq!(reviewer["backend"], "claude");
        assert_eq!(reviewer["role"], "code-reviewer");
        assert_eq!(reviewer["instructions"], "./reviewer-instructions.md");
        // Daemon's contribution landed.
        assert_eq!(reviewer["source_repo"], "/Users/dev/.agend");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merge_new_instance_creates_fresh_entry() {
        // Daemon adds a brand-new instance not in fleet.yaml — fresh
        // mapping created from the entry.
        let home = merge_tmp_home("new-instance");
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  existing:\n    backend: claude\n",
        )
        .unwrap();
        let entry = InstanceYamlEntry {
            backend: Some("kiro-cli".into()),
            role: Some("auditor".into()),
            ..Default::default()
        };
        add_instances_to_yaml(&home, &[("auditor", &entry)]).unwrap();

        let v = read_yaml(&home);
        assert_eq!(v["instances"]["auditor"]["backend"], "kiro-cli");
        assert_eq!(v["instances"]["auditor"]["role"], "auditor");
        // Existing instance untouched.
        assert_eq!(v["instances"]["existing"]["backend"], "claude");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merge_daemon_managed_overwrite_does_not_emit_conflict_error() {
        // Defensive bonus: even when operator hand-edited a
        // DaemonManaged field with a different value, the merge
        // overwrites silently (no 真衝突 error). Daemon-managed
        // fields are AUTHORITATIVE — the operator can't object,
        // and the merge contract says daemon wins.
        let home = merge_tmp_home("daemon-managed-no-error");
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  dev:\n    backend: claude\n    source_repo: /operator-edit\n",
        )
        .unwrap();
        let entry = InstanceYamlEntry {
            source_repo: Some("/daemon-authoritative".into()),
            ..Default::default()
        };
        let result = add_instances_to_yaml(&home, &[("dev", &entry)]);
        assert!(
            result.is_ok(),
            "DaemonManaged field overwrite MUST NOT 真衝突: {:?}",
            result
        );
        let v = read_yaml(&home);
        assert_eq!(
            v["instances"]["dev"]["source_repo"], "/daemon-authoritative",
            "DaemonManaged value should be authoritative"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merge_preserves_yaml_field_ordering_for_operator_readability() {
        // Defensive bonus: the merge must preserve YAML key ordering
        // (serde_yaml_ng's IndexMap-backed Mapping does this by
        // default). Pin the behaviour so a future refactor doesn't
        // accidentally switch to a HashMap-backed mapping that would
        // randomize order on every write.
        let home = merge_tmp_home("ordering");
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  dev:\n    role: developer\n    backend: claude\n    \
             working_directory: /workspace/dev\n",
        )
        .unwrap();

        // Daemon writes nothing new — but the round-trip should
        // preserve the original key order.
        let entry = InstanceYamlEntry {
            backend: Some("claude".into()),
            ..Default::default()
        };
        add_instances_to_yaml(&home, &[("dev", &entry)]).unwrap();

        let content = std::fs::read_to_string(fleet_yaml_path(&home)).unwrap();
        // role should come before backend in the rewritten file.
        let role_pos = content.find("role:").expect("role present");
        let backend_pos = content.find("backend:").expect("backend present");
        assert!(
            role_pos < backend_pos,
            "field order should be preserved across merge round-trip; got:\n{content}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merge_handles_multiple_instances_atomically() {
        // Defensive bonus: the merge function operates on multiple
        // instances in one lock+write cycle. If ANY instance has
        // a 真衝突, the entire write should be rejected (or all
        // succeed). Pin the all-or-nothing semantic.
        let home = merge_tmp_home("multi-atomic");
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  alpha:\n    role: senior-dev\n  beta:\n    role: junior-dev\n",
        )
        .unwrap();

        let entry_alpha = InstanceYamlEntry {
            role: Some("lead".into()), // conflict with senior-dev
            ..Default::default()
        };
        let entry_beta = InstanceYamlEntry {
            role: Some("junior-dev".into()), // identical, no conflict
            ..Default::default()
        };
        let result =
            add_instances_to_yaml(&home, &[("alpha", &entry_alpha), ("beta", &entry_beta)]);
        // Alpha conflict surfaces — but the conflict mention should
        // still indicate which instance.
        assert!(result.is_err(), "any conflict must abort the whole batch");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("alpha"),
            "conflict report must name the conflicting instance; got: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #790: `display_timezone` survives a load → save round-trip.
    /// Operator-set value persists across daemon restarts; absent
    /// stays absent (`skip_serializing_if = Option::is_none` keeps
    /// existing fleet.yaml clean).
    #[test]
    fn fleet_config_load_save_roundtrip_preserves_display_timezone() {
        let dir = std::env::temp_dir().join(format!("agend-tz-rt-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            "display_timezone: Asia/Taipei\ninstances:\n  shell:\n    command: /bin/bash\n",
        );
        let loaded = FleetConfig::load(&path).expect("load");
        assert_eq!(loaded.display_timezone.as_deref(), Some("Asia/Taipei"));

        let yaml = serde_yaml_ng::to_string(&loaded).expect("serialize");
        let reloaded: FleetConfig = serde_yaml_ng::from_str(&yaml).expect("deserialize");
        assert_eq!(reloaded.display_timezone.as_deref(), Some("Asia/Taipei"));

        // Absent → stays absent on serialize (skip_serializing_if).
        let bare_path = write_fleet(
            &dir.join("bare"),
            "instances:\n  shell:\n    command: /bin/bash\n",
        );
        let bare = FleetConfig::load(&bare_path).expect("load bare");
        assert!(bare.display_timezone.is_none());
        let bare_yaml = serde_yaml_ng::to_string(&bare).expect("serialize bare");
        assert!(
            !bare_yaml.contains("display_timezone"),
            "absent field must not be serialized, got:\n{bare_yaml}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
