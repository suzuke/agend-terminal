use crate::backend::Backend;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetConfig {
    #[serde(default)]
    pub defaults: InstanceDefaults,
    #[serde(default)]
    pub instances: HashMap<String, InstanceConfig>,
    #[serde(default)]
    pub teams: HashMap<String, TeamConfig>,
    /// Channel configuration (e.g., Telegram).
    pub channel: Option<ChannelConfig>,
    /// Template definitions for batch deployment.
    #[serde(default)]
    pub templates: Option<HashMap<String, serde_yaml::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChannelConfig {
    #[serde(rename = "telegram")]
    Telegram {
        /// Env var name containing the bot token.
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
    },
}

fn default_mode() -> String {
    "topic".to_string()
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
    /// Backend preset name — overrides defaults.backend.
    pub backend: Option<Backend>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    pub working_directory: Option<String>,
    pub ready_pattern: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub topic_id: Option<i32>,
    /// Custom git branch name for worktree. TS version uses "worktree_source".
    #[serde(alias = "worktree_source")]
    pub git_branch: Option<String>,
    /// Model override (e.g., "opus", "sonnet"). Passed as --model flag.
    pub model: Option<String>,
    /// Display name for UI/Telegram.
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConfig {
    #[serde(default)]
    pub members: Vec<String>,
}

impl FleetConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read fleet config: {}", path.display()))?;
        let mut config: FleetConfig = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse fleet config: {}", path.display()))?;
        config.normalize();
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

        // Submit key comes straight from the backend's preset. Shell/Raw
        // default to `\r`.
        let submit_key = preset.submit_key.to_string();

        let working_directory = Some(if let Some(d) = inst.working_directory.as_ref() {
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
        })
    }

    /// Get all instance names.
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
}

fn dirs_home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Entry for adding a dynamic instance to fleet.yaml.
pub struct InstanceYamlEntry {
    pub backend: Option<String>,
    pub working_directory: Option<String>,
    pub role: Option<String>,
}

/// Atomically write a serde_yaml::Value back to fleet.yaml using temp + fsync + rename.
/// Caller must hold the file lock.
fn atomic_write_yaml(home: &Path, doc: &serde_yaml::Value) -> Result<()> {
    let yaml = serde_yaml::to_string(doc).context("Failed to serialize fleet.yaml")?;
    let fleet_path = home.join("fleet.yaml");
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
    mutate: impl FnOnce(&mut serde_yaml::Value) -> Result<()>,
) -> Result<()> {
    let fleet_path = home.join("fleet.yaml");
    if default_content.is_empty() && !fleet_path.exists() {
        return Ok(());
    }
    let _lock = acquire_lock(home)?;
    let content =
        std::fs::read_to_string(&fleet_path).unwrap_or_else(|_| default_content.to_string());
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(&content).context("Failed to parse fleet.yaml")?;
    mutate(&mut doc)?;
    atomic_write_yaml(home, &doc)
}

/// Add a new instance entry to fleet.yaml. Uses file lock + atomic write.
pub fn add_instance_to_yaml(home: &Path, name: &str, config: &InstanceYamlEntry) -> Result<()> {
    add_instances_to_yaml(home, &[(name, config)])
}

/// Add multiple instance entries to fleet.yaml in a single lock+write cycle.
pub fn add_instances_to_yaml(home: &Path, entries: &[(&str, &InstanceYamlEntry)]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    mutate_fleet_yaml(home, "instances: {}\n", |doc| {
        if doc.get("instances").is_none() {
            doc["instances"] = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        }
        let instances = doc
            .get_mut("instances")
            .and_then(|v| v.as_mapping_mut())
            .context("instances is not a mapping")?;

        for (name, config) in entries {
            let mut inst = serde_yaml::Mapping::new();
            for (key, val) in [
                ("backend", &config.backend),
                ("working_directory", &config.working_directory),
                ("role", &config.role),
            ] {
                if let Some(ref v) = val {
                    inst.insert(key.into(), serde_yaml::Value::String(v.clone()));
                }
            }
            instances.insert(
                serde_yaml::Value::String(name.to_string()),
                serde_yaml::Value::Mapping(inst),
            );
            tracing::info!(%name, "added instance to fleet.yaml");
        }
        Ok(())
    })
}

/// Remove an instance entry from fleet.yaml. Uses file lock + atomic write.
pub fn remove_instance_from_yaml(home: &Path, name: &str) -> Result<()> {
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) {
            instances.remove(serde_yaml::Value::String(name.to_string()));
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
                instances.remove(serde_yaml::Value::String(name.clone()));
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
    value: serde_yaml::Value,
) -> Result<()> {
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) {
            let key = serde_yaml::Value::String(name.to_string());
            if let Some(inst) = instances.get_mut(&key).and_then(|v| v.as_mapping_mut()) {
                inst.insert(serde_yaml::Value::String(field.to_string()), value);
            }
        }
        Ok(())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

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
        };
        add_instance_to_yaml(&dir, "new-agent", &entry).expect("add");
        let config = FleetConfig::load(&path).expect("load after add");
        assert!(config.instances.contains_key("new-agent"));
        let inst = &config.instances["new-agent"];
        assert_eq!(inst.backend, Some(crate::backend::Backend::ClaudeCode));
        assert_eq!(inst.working_directory.as_deref(), Some("/tmp/work"));
        assert_eq!(inst.role.as_deref(), Some("developer"));
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
            serde_yaml::Value::Number(serde_yaml::Number::from(42)),
        )
        .expect("update field");
        let config = FleetConfig::load(&dir.join("fleet.yaml")).expect("load");
        assert_eq!(config.instances["agent1"].topic_id, Some(42));

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
        }

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
}
