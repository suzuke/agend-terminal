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
    },
}

fn default_mode() -> String {
    "topic".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceDefaults {
    /// Backend preset name (e.g., "claude-code", "kiro-cli").
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceConfig {
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
    /// Custom git branch name for worktree (overrides agend/{name}).
    pub git_branch: Option<String>,
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
        let config: FleetConfig = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse fleet config: {}", path.display()))?;
        Ok(config)
    }

    /// Resolve an instance config by merging with defaults + backend preset.
    pub fn resolve_instance(&self, name: &str) -> Option<ResolvedInstance> {
        let inst = self.instances.get(name)?;
        let defaults = &self.defaults;

        // Backend preset: instance > defaults
        let backend = inst.backend.as_ref().or(defaults.backend.as_ref());
        let preset = backend.map(|b| b.preset());

        // Command: instance > defaults > preset > "claude"
        let command = inst
            .command
            .clone()
            .or_else(|| defaults.command.clone())
            .or_else(|| preset.as_ref().map(|p| p.command.to_string()))
            .unwrap_or_else(|| "claude".to_string());

        // Args: instance > defaults > preset (only if command basename matches preset)
        let command_basename = std::path::Path::new(&command)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(&command)
            .to_lowercase();
        let command_matches_preset = preset
            .as_ref()
            .map(|p| {
                command_basename == p.command
                    || command_basename.starts_with(&format!("{}-", p.command))
            })
            .unwrap_or(false);
        let args = if !inst.args.is_empty() {
            inst.args.clone()
        } else if !defaults.args.is_empty() {
            defaults.args.clone()
        } else if let Some(ref p) = preset {
            if command_matches_preset {
                p.args.iter().map(|s| s.to_string()).collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // Merge env: defaults first, then instance overrides
        let mut env = defaults.env.clone();
        env.extend(inst.env.clone());
        env.insert("AGEND_INSTANCE_NAME".to_string(), name.to_string());

        // Ready pattern: instance > defaults > preset
        let ready_pattern = inst
            .ready_pattern
            .clone()
            .or_else(|| defaults.ready_pattern.clone())
            .or_else(|| preset.as_ref().map(|p| p.ready_pattern.to_string()));

        // Submit key: from preset (only if command matches) or default \r
        let submit_key = if command_matches_preset {
            preset
                .as_ref()
                .map(|p| p.submit_key.to_string())
                .unwrap_or_else(|| "\r".to_string())
        } else {
            "\r".to_string()
        };

        let working_directory = inst.working_directory.as_ref().map(|d| {
            // Expand ~ to home directory
            if let Some(rest) = d.strip_prefix("~/") {
                if let Some(home) = dirs_home() {
                    return home.join(rest);
                }
            }
            PathBuf::from(d)
        });

        let cols = inst.cols.or(defaults.cols);
        let rows = inst.rows.or(defaults.rows);

        Some(ResolvedInstance {
            name: name.to_string(),
            command,
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
    pub command: String,
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
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Entry for adding a dynamic instance to fleet.yaml.
pub struct InstanceYamlEntry {
    pub command: String,
    pub backend: Option<String>,
    pub working_directory: Option<String>,
    pub role: Option<String>,
}

/// Atomically write a serde_yaml::Value back to fleet.yaml using temp file + rename.
/// Caller must hold the file lock.
fn atomic_write_yaml(home: &Path, doc: &serde_yaml::Value) -> Result<()> {
    let yaml = serde_yaml::to_string(doc).context("Failed to serialize fleet.yaml")?;
    let fleet_path = home.join("fleet.yaml");
    let tmp_path = home.join(".fleet.yaml.tmp");
    std::fs::write(&tmp_path, &yaml).context("Failed to write temp fleet.yaml")?;
    std::fs::rename(&tmp_path, &fleet_path).context("Failed to rename temp fleet.yaml")?;
    Ok(())
}

/// Acquire the fleet.yaml file lock via flock (auto-released on crash/drop).
fn acquire_lock(home: &Path) -> Result<nix::fcntl::Flock<std::fs::File>> {
    let lock_path = home.join(".fleet.yaml.lock");
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&lock_path)
        .context("failed to open lock file")?;
    nix::fcntl::Flock::lock(f, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, e)| anyhow::anyhow!("flock failed: {e}"))
    // Lock auto-released when Flock is dropped
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
    mutate_fleet_yaml(home, "instances: {}\n", |doc| {
        if doc.get("instances").is_none() {
            doc["instances"] = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        }
        let instances = doc
            .get_mut("instances")
            .and_then(|v| v.as_mapping_mut())
            .context("instances is not a mapping")?;

        let mut inst = serde_yaml::Mapping::new();
        inst.insert(
            "command".into(),
            serde_yaml::Value::String(config.command.clone()),
        );
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
        eprintln!("[fleet] added instance '{name}' to fleet.yaml");
        Ok(())
    })
}

/// Remove an instance entry from fleet.yaml. Uses file lock + atomic write.
pub fn remove_instance_from_yaml(home: &Path, name: &str) -> Result<()> {
    mutate_fleet_yaml(home, "", |doc| {
        if let Some(instances) = doc.get_mut("instances").and_then(|v| v.as_mapping_mut()) {
            instances.remove(serde_yaml::Value::String(name.to_string()));
        }
        eprintln!("[fleet] removed instance '{name}' from fleet.yaml");
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
  backend: claude-code
instances:
  test:
    command: /bin/bash
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("test").expect("resolve");

        assert_eq!(resolved.command, "/bin/bash");
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
    fn test_preset_args_applied_to_matching_command() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-test2-{}", std::process::id()));
        let path = write_fleet(
            &dir,
            r#"
defaults:
  backend: claude-code
instances:
  test:
    command: claude
"#,
        );
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("test").expect("resolve");

        assert_eq!(resolved.command, "claude");
        assert!(!resolved.args.is_empty(), "preset args should be applied");
        assert!(resolved
            .args
            .contains(&"--dangerously-skip-permissions".to_string()));

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
            command: "claude".to_string(),
            backend: Some("claude-code".to_string()),
            working_directory: Some("/tmp/work".to_string()),
            role: Some("developer".to_string()),
        };
        add_instance_to_yaml(&dir, "new-agent", &entry).expect("add");
        let config = FleetConfig::load(&path).expect("load after add");
        assert!(config.instances.contains_key("new-agent"));
        let inst = &config.instances["new-agent"];
        assert_eq!(inst.command.as_deref(), Some("claude"));
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
            command: "claude".to_string(),
            backend: None,
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
        assert_eq!(resolved.command, "/bin/bash");

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
            command: "test-cmd".to_string(),
            backend: None,
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
        // Should end with /project
        assert!(
            wd.to_string_lossy().ends_with("/project"),
            "should end with /project, got: {}",
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
    backend: claude-code
    topic_id: 229
  general:
    backend: claude-code
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
    backend: claude-code
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
    backend: claude-code
    topic_id: 229
  bob:
    backend: claude-code
    topic_id: 300
"#,
        )
        .ok();
        remove_instance_from_yaml(&dir, "alice").expect("remove");
        let config = FleetConfig::load(&path).expect("load");
        assert!(config.instances.get("alice").is_none());
        assert_eq!(
            config.instances.get("bob").and_then(|i| i.topic_id),
            Some(300)
        );
        fs::remove_dir_all(&dir).ok();
    }
}
