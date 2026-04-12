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

        // Args: instance > defaults > preset (only if command matches preset command)
        let command_matches_preset = preset.as_ref()
            .map(|p| command.contains(p.command))
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
            preset.as_ref()
                .map(|p| p.submit_key.to_string())
                .unwrap_or_else(|| "\r".to_string())
        } else {
            "\r".to_string()
        };

        let working_directory = inst.working_directory.as_ref().map(|d| {
            // Expand ~ to home directory
            if d.starts_with("~/") {
                if let Some(home) = dirs_home() {
                    return home.join(&d[2..]);
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
        let path = write_fleet(&dir, r#"
defaults:
  backend: claude-code
instances:
  test:
    command: /bin/bash
"#);
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("test").expect("resolve");

        assert_eq!(resolved.command, "/bin/bash");
        // Preset args (--dangerously-skip-permissions) should NOT be applied
        assert!(resolved.args.is_empty(), "args should be empty for non-preset command, got: {:?}", resolved.args);
        // Submit key should be default \r, not preset's
        assert_eq!(resolved.submit_key, "\r");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_preset_args_applied_to_matching_command() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-test2-{}", std::process::id()));
        let path = write_fleet(&dir, r#"
defaults:
  backend: claude-code
instances:
  test:
    command: claude
"#);
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("test").expect("resolve");

        assert_eq!(resolved.command, "claude");
        assert!(!resolved.args.is_empty(), "preset args should be applied");
        assert!(resolved.args.contains(&"--dangerously-skip-permissions".to_string()));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_env_merge_order() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-test3-{}", std::process::id()));
        let path = write_fleet(&dir, r#"
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
"#);
        let config = FleetConfig::load(&path).expect("load");
        let resolved = config.resolve_instance("test").expect("resolve");

        assert_eq!(resolved.env.get("KEY1").map(|s| s.as_str()), Some("default_val"));
        assert_eq!(resolved.env.get("KEY2").map(|s| s.as_str()), Some("instance_val")); // instance overrides
        assert_eq!(resolved.env.get("KEY3").map(|s| s.as_str()), Some("instance_only"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_git_branch_override() {
        let dir = std::env::temp_dir().join(format!("agend-fleet-test4-{}", std::process::id()));
        let path = write_fleet(&dir, r#"
instances:
  with_branch:
    command: /bin/bash
    git_branch: "custom/branch"
  without_branch:
    command: /bin/bash
"#);
        let config = FleetConfig::load(&path).expect("load");

        let with = config.resolve_instance("with_branch").expect("resolve");
        assert_eq!(with.git_branch.as_deref(), Some("custom/branch"));

        let without = config.resolve_instance("without_branch").expect("resolve");
        assert!(without.git_branch.is_none());

        fs::remove_dir_all(&dir).ok();
    }
}
