pub(crate) mod merge;
pub(crate) mod persist;
mod resolve;

#[allow(unused_imports)]
pub use persist::{
    add_instance_to_yaml, add_instances_to_yaml, add_team_to_yaml, migrate_teams_json_to_yaml,
    remove_instance_from_yaml, remove_instances_from_yaml, remove_team_from_yaml,
    update_channel_telegram_group_id, update_instance_field, update_team_in_yaml,
};

use crate::backend::Backend;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Mtime-based cache for `FleetConfig::load()`.
/// Avoids repeated `read_to_string` + `serde_yaml_ng` parse when the
/// file hasn't changed on disk.
static FLEET_CACHE: std::sync::Mutex<Option<FleetCacheEntry>> = std::sync::Mutex::new(None);

struct FleetCacheEntry {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
    config: FleetConfig,
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
    #[serde(skip)]
    pub(crate) home: Option<PathBuf>,
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
    pub instructions: Option<String>,
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
    pub fn load(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path).ok();
        let disk_mtime = meta.as_ref().and_then(|m| m.modified().ok());
        let disk_size = meta.as_ref().map(|m| m.len());

        if let (Some(mtime), Some(size)) = (disk_mtime, disk_size) {
            let guard = FLEET_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = guard.as_ref() {
                if entry.path == path && entry.mtime == mtime && entry.size == size {
                    return Ok(entry.config.clone());
                }
            }
        }

        let config = Self::load_uncached(path)?;

        if let (Some(mtime), Some(size)) = (disk_mtime, disk_size) {
            let mut guard = FLEET_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(FleetCacheEntry {
                path: path.to_path_buf(),
                mtime,
                size,
                config: config.clone(),
            });
        }

        Ok(config)
    }

    fn load_uncached(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read fleet config: {}", path.display()))?;
        let mut config: FleetConfig = serde_yaml_ng::from_str(&content)
            .with_context(|| format!("Failed to parse fleet config: {}", path.display()))?;
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
}

// Persistence, merge, and team mutation functions live in fleet::persist and fleet::merge.
// Re-exported at module level for API compatibility.

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
            topic_binding_mode: None,
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
            topic_binding_mode: None,
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

    #[test]
    fn topic_binding_mode_round_trips_through_fleet_yaml() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-tb-rt-{}", std::process::id()));
        write_fleet(&dir, "instances: {}\n");
        let entry = InstanceYamlEntry {
            backend: Some("claude".to_string()),
            topic_binding_mode: Some("skip".to_string()),
            ..Default::default()
        };
        add_instance_to_yaml(&dir, "internal-helper", &entry).expect("add");
        let content = std::fs::read_to_string(dir.join("fleet.yaml")).expect("read");
        assert!(
            content.contains("topic_binding_mode: skip"),
            "topic_binding_mode must appear in fleet.yaml: {content}"
        );
        let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        let inst = config.instances.get("internal-helper").expect("exists");
        assert_eq!(
            inst.topic_binding_mode.as_deref(),
            Some("skip"),
            "topic_binding_mode must round-trip through serde"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn topic_binding_mode_absent_parses_as_none() {
        let dir =
            std::env::temp_dir().join(format!("agend-fleet-tb-absent-{}", std::process::id()));
        write_fleet(&dir, "instances:\n  agent1:\n    backend: claude\n");
        let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        let inst = config.instances.get("agent1").expect("exists");
        assert!(
            inst.topic_binding_mode.is_none(),
            "absent field must parse as None for back-compat"
        );
        fs::remove_dir_all(&dir).ok();
    }

    // ─────────────────────────────────────────────────────────────
    // ── #962 silent-persist failure tests (Layer 1 internal tracing) ──
    //
    // Each test pins one of the 3 documented silent no-op paths inside
    // `update_instance_field`. Pre-#962 all three returned `Ok(())` and
    // callers had no way to distinguish persisted from silently-dropped.
    // Post-#962 they return `Ok(false)` and emit `tracing::warn!` with
    // a stable `reason` field.

    fn tmp_home_962(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-962-{}-{}-{}", tag, std::process::id(), id));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn update_instance_field_returns_false_when_fleet_yaml_absent() {
        let home = tmp_home_962("absent");
        // No fleet.yaml planted.
        let result = update_instance_field(
            &home,
            "any-agent",
            "topic_id",
            serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(42)),
        );
        assert!(
            matches!(result, Ok(false)),
            "missing fleet.yaml must return Ok(false), got {result:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_instance_field_returns_false_when_instance_entry_missing() {
        let home = tmp_home_962("entry-missing");
        // fleet.yaml exists but has no entry for "ghost".
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  alpha:\n    backend: claude\n",
        )
        .unwrap();
        let result = update_instance_field(
            &home,
            "ghost",
            "topic_id",
            serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(42)),
        );
        assert!(
            matches!(result, Ok(false)),
            "missing instance entry must return Ok(false), got {result:?}"
        );
        // Confirm fleet.yaml unchanged.
        let body = std::fs::read_to_string(fleet_yaml_path(&home)).unwrap();
        assert!(!body.contains("ghost"), "ghost entry must NOT be inserted");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_instance_field_returns_false_when_not_mapping() {
        let home = tmp_home_962("not-mapping");
        // Instance entry exists but is a SCALAR (string), not a mapping.
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  alpha: just-a-string\n",
        )
        .unwrap();
        let result = update_instance_field(
            &home,
            "alpha",
            "topic_id",
            serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(42)),
        );
        assert!(
            matches!(result, Ok(false)),
            "non-mapping entry must return Ok(false), got {result:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_instance_field_returns_true_on_successful_persist() {
        let home = tmp_home_962("happy");
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  alpha:\n    backend: claude\n",
        )
        .unwrap();
        let result = update_instance_field(
            &home,
            "alpha",
            "topic_id",
            serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(123)),
        );
        assert!(
            matches!(result, Ok(true)),
            "happy path must return Ok(true), got {result:?}"
        );
        let body = std::fs::read_to_string(fleet_yaml_path(&home)).unwrap();
        assert!(
            body.contains("topic_id: 123"),
            "topic_id must be persisted; got:\n{body}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[tracing_test::traced_test]
    #[test]
    fn update_instance_field_emits_warn_with_reason_on_each_no_op_path() {
        // Path 2: instance entry missing.
        let home = tmp_home_962("warn-entry-missing");
        std::fs::write(
            fleet_yaml_path(&home),
            "instances:\n  alpha:\n    backend: claude\n",
        )
        .unwrap();
        let _ = update_instance_field(
            &home,
            "ghost",
            "topic_id",
            serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(42)),
        );
        assert!(
            logs_contain("update_instance_field skipped"),
            "tracing::warn! must fire on silent no-op path"
        );
        assert!(
            logs_contain("instance_entry_missing"),
            "tracing reason must identify the specific no-op path"
        );
        assert!(logs_contain("ghost"), "tracing must carry instance name");
        assert!(logs_contain("topic_id"), "tracing must carry field name");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #964 regression anchor — promoted from dev's bisect spike at
    /// `/tmp/bisect-964-dev.md`. Documents the SILENT no-op contract:
    /// when `update_instance_field` is called against a missing entry, it
    /// MUST return `Ok(false)` and leave fleet.yaml unchanged. The #964
    /// fix is caller-side (`spawn_single_instance` adds the entry BEFORE
    /// SPAWN so the SPAWN-time `register_topic` chain finds it) — this
    /// test pins the helper's intentional contract so a future
    /// well-meaning refactor to "auto-insert on missing" doesn't silently
    /// re-introduce the bootstrap-backfill ambiguity that masked #964 for
    /// 27 days.
    ///
    /// Sibling: caller-side regression tests at
    /// `src/mcp/handlers/instance.rs::tests_964::t1_create_instance_persists_topic_id_to_fleet_yaml`.
    #[test]
    fn repro_964_helper_silently_no_ops_on_empty_fleet_yaml() {
        let home = tmp_home_962("repro-964");
        // Plant fleet.yaml with explicitly NO entry for the target name.
        std::fs::write(fleet_yaml_path(&home), "instances: {}\n").unwrap();

        let result = update_instance_field(
            &home,
            "test-964-verify",
            "topic_id",
            serde_yaml_ng::Value::Number(serde_yaml_ng::Number::from(5198)),
        );

        assert!(
            matches!(result, Ok(false)),
            "#964 anchor: helper MUST silently no-op (Ok(false)) on \
             missing entry; the fix lives in the caller. Got: {result:?}"
        );

        let cfg = FleetConfig::load(&fleet_yaml_path(&home)).expect("reload");
        assert!(
            !cfg.instances.contains_key("test-964-verify"),
            "#964 anchor: helper MUST NOT auto-insert; got entry {:?}",
            cfg.instances.get("test-964-verify")
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn load_cache_returns_same_result_on_unchanged_file() {
        let _g = env_guard();
        invalidate_cache();
        let dir =
            std::env::temp_dir().join(format!("agend-fleet-cache-hit-{}", std::process::id()));
        let path = write_fleet(&dir, "instances:\n  cached-agent:\n    backend: claude\n");
        let first = FleetConfig::load(&path).expect("first load");
        let second = FleetConfig::load(&path).expect("second load (cached)");
        assert_eq!(first.instances.len(), second.instances.len());
        assert!(second.instances.contains_key("cached-agent"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_cache_invalidates_on_file_change() {
        let _g = env_guard();
        invalidate_cache();
        let dir =
            std::env::temp_dir().join(format!("agend-fleet-cache-inv-{}", std::process::id()));
        let path = write_fleet(&dir, "instances:\n  old-agent:\n    backend: claude\n");
        let first = FleetConfig::load(&path).expect("first load");
        assert!(first.instances.contains_key("old-agent"));

        // Ensure mtime changes (some filesystems have 1-second granularity)
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&path, "instances:\n  new-agent:\n    backend: claude\n").expect("rewrite");

        let second = FleetConfig::load(&path).expect("second load (invalidated)");
        assert!(
            !second.instances.contains_key("old-agent"),
            "old-agent should be gone after rewrite"
        );
        assert!(
            second.instances.contains_key("new-agent"),
            "new-agent should appear after rewrite"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_cache_detects_same_mtime_different_size() {
        let _g = env_guard();
        invalidate_cache();
        let dir =
            std::env::temp_dir().join(format!("agend-fleet-cache-size-{}", std::process::id()));
        let path = write_fleet(&dir, "instances:\n  a:\n    backend: claude\n");
        let first = FleetConfig::load(&path).expect("first load");
        assert!(first.instances.contains_key("a"));

        // Rewrite with different content (different size) without sleeping —
        // mtime may be identical on coarse-grained filesystems.
        std::fs::write(
            &path,
            "instances:\n  longer-name-agent:\n    backend: claude\n",
        )
        .expect("rewrite");

        let second = FleetConfig::load(&path).expect("second load");
        // If size changed, cache must invalidate even with same mtime.
        // If mtime also changed, cache invalidates too. Either way: correct.
        assert!(
            second.instances.contains_key("longer-name-agent"),
            "different-size rewrite must not return stale cache"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mutate_path_invalidates_cache() {
        let _g = env_guard();
        invalidate_cache();
        let dir =
            std::env::temp_dir().join(format!("agend-fleet-cache-mutate-{}", std::process::id()));
        let path = write_fleet(&dir, "instances:\n  before:\n    backend: claude\n");
        let first = FleetConfig::load(&path).expect("first load");
        assert!(first.instances.contains_key("before"));

        add_instance_to_yaml(
            &dir,
            "after",
            &InstanceYamlEntry {
                backend: Some("claude-code".to_string()),
                ..Default::default()
            },
        )
        .expect("add instance");

        let second = FleetConfig::load(&path).expect("load after mutate");
        assert!(
            second.instances.contains_key("after"),
            "cache must be invalidated by atomic_write_yaml"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_instance_inherits_defaults_instructions() {
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-def-instr-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = write_fleet(
            &dir,
            r#"
defaults:
  instructions: shared.md
instances:
  agent1:
    command: /bin/bash
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("agent1").expect("resolve");
        assert_eq!(
            resolved.instructions.as_deref(),
            Some("shared.md"),
            "defaults.instructions must propagate when instance omits it"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_instance_instructions_override_beats_defaults() {
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-instr-override-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = write_fleet(
            &dir,
            r#"
defaults:
  instructions: shared.md
instances:
  agent1:
    command: /bin/bash
    instructions: custom.md
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("agent1").expect("resolve");
        assert_eq!(
            resolved.instructions.as_deref(),
            Some("custom.md"),
            "instance instructions must override defaults"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_instance_no_instructions_stays_none() {
        let dir = std::env::temp_dir().join(format!(
            "agend-fleet-no-instr-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = write_fleet(
            &dir,
            r#"
instances:
  agent1:
    command: /bin/bash
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("agent1").expect("resolve");
        assert_eq!(
            resolved.instructions, None,
            "no instructions anywhere must remain None"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
