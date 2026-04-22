//! Fleet config normalization: side effects that must happen before any
//! agent spawns, regardless of entry point (`start` or `app`).
//!
//! - Auto-create the `general` coordinator when `channel:` is configured but
//!   `general` is missing. Bound to Telegram General topic (topic_id 1) and
//!   persisted to fleet.yaml so the operator sees the change.
//! - Prune stale git worktrees across every instance's working directory.
//!
//! The `persist` flag gates fleet.yaml mutations so verifier/CI contexts can
//! normalize in memory without touching disk.

use crate::backend::Backend;
use crate::fleet::{self, FleetConfig, InstanceConfig, InstanceYamlEntry};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Normalize in-memory, optionally persisting fleet.yaml side effects.
pub(super) fn normalize(config: &mut FleetConfig, home: &Path, persist: bool) {
    auto_create_general(config, home, persist);
    walk_resolved_dirs(config);
}

fn auto_create_general(config: &mut FleetConfig, home: &Path, persist: bool) {
    if config.channel.is_none() || config.instances.contains_key("general") {
        return;
    }
    let default_backend = config
        .defaults
        .backend
        .clone()
        .unwrap_or(Backend::ClaudeCode);
    config.instances.insert(
        "general".to_string(),
        InstanceConfig {
            role: Some("Fleet coordinator — routes tasks between agents".to_string()),
            backend: Some(default_backend),
            working_directory: None,
            topic_id: Some(1),
            ..Default::default()
        },
    );

    if !persist {
        return;
    }

    let entry = InstanceYamlEntry {
        backend: config
            .defaults
            .backend
            .as_ref()
            .map(|b| b.name().to_string()),
        working_directory: None,
        role: Some("Fleet coordinator — routes tasks between agents".to_string()),
    };
    if let Err(e) = fleet::add_instance_to_yaml(home, "general", &entry) {
        tracing::warn!(error = %e, "failed to persist general instance");
    }
    let _ = fleet::update_instance_field(
        home,
        "general",
        "topic_id",
        serde_yaml::Value::Number(serde_yaml::Number::from(1)),
    );
    tracing::info!("auto-created 'general' instance for channel");
}

/// Single walk of resolved instances: prunes each unique working_directory's
/// stale worktrees, and groups instance names by directory so we can warn
/// about shared non-git cwds where resume modes would collide.
fn walk_resolved_dirs(config: &FleetConfig) {
    let mut groups: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for name in config.instance_names() {
        if let Some(resolved) = config.resolve_instance(&name) {
            if let Some(dir) = resolved.working_directory {
                groups.entry(dir).or_default().push(resolved.name);
            }
        }
    }
    for (dir, names) in &groups {
        crate::worktree::prune(dir);
        if names.len() >= 2 && !crate::worktree::is_git_repo(dir) {
            warn_shared_cwd_once(dir, names);
        }
    }
}

/// Shared cwd without git isolation will collide on resume (all cwd-scoped
/// `--continue` / `--resume` pick "most recent session in cwd"). Emit once
/// per directory so hot-reloads don't spam the log with the same warning.
fn warn_shared_cwd_once(dir: &Path, names: &[String]) {
    static WARNED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
    let mut guard = crate::sync::lock_poisoned(&WARNED, "shared_cwd_warned");
    let set = guard.get_or_insert_with(HashSet::new);
    if !set.insert(dir.to_path_buf()) {
        return;
    }
    tracing::warn!(
        dir = %dir.display(),
        instances = ?names,
        "multiple instances share a non-git working_directory; resume is cwd-scoped and will latest-wins across them. Either `git init` the directory (auto-worktree then isolates each instance) or assign distinct working_directory values per instance."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::ChannelConfig;

    fn minimal_with_channel() -> FleetConfig {
        let mut c = FleetConfig {
            defaults: crate::fleet::InstanceDefaults {
                backend: Some(Backend::ClaudeCode),
                ..Default::default()
            },
            instances: Default::default(),
            teams: Default::default(),
            channel: Some(ChannelConfig::Telegram {
                bot_token_env: "TG_TOKEN".into(),
                group_id: -1,
                mode: "topic".into(),
                user_allowlist: None,
            }),
            channels: None,
            templates: None,
        };
        c.instances.insert(
            "worker".to_string(),
            InstanceConfig {
                backend: Some(Backend::ClaudeCode),
                ..Default::default()
            },
        );
        c
    }

    #[test]
    fn injects_general_when_channel_configured() {
        let mut c = minimal_with_channel();
        assert!(!c.instances.contains_key("general"));
        normalize(&mut c, Path::new("/tmp/agend-normalize-test"), false);
        let g = c.instances.get("general").expect("general inserted");
        assert_eq!(g.topic_id, Some(1));
    }

    #[test]
    fn skips_general_when_no_channel() {
        let mut c = minimal_with_channel();
        c.channel = None;
        normalize(&mut c, Path::new("/tmp/agend-normalize-test"), false);
        assert!(!c.instances.contains_key("general"));
    }

    #[test]
    fn leaves_existing_general_alone() {
        let mut c = minimal_with_channel();
        c.instances.insert(
            "general".to_string(),
            InstanceConfig {
                backend: Some(Backend::ClaudeCode),
                topic_id: Some(42),
                ..Default::default()
            },
        );
        normalize(&mut c, Path::new("/tmp/agend-normalize-test"), false);
        assert_eq!(
            c.instances.get("general").and_then(|i| i.topic_id),
            Some(42)
        );
    }
}
