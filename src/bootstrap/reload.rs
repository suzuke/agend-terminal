//! Hot-reload for fleet.yaml.
//!
//! Polls fleet.yaml mtime from the daemon main-loop tick. On change:
//! - **Added** instances are spawned in-place (resolve_one + agent::spawn_agent +
//!   serve_agent_tui + register config for respawn).
//! - **Removed / command-changed / args-changed / working-dir-changed** are
//!   warn-only — replacing a live PTY risks losing user state (ongoing agent
//!   session, open editor, etc.), so the operator must explicitly delete or
//!   replace the instance.
//! - **Role / topic_id** changes are logged and left for a future in-place
//!   update (instructions regen + registry metadata swap). Not done here
//!   because the runtime doesn't consume those fields after spawn; re-running
//!   `instructions::generate` would rewrite files an agent is actively
//!   reading, so we log and skip until there's a safe story.
//!
//! The pure-function diff sits at the core so the policy is unit-testable
//! without bringing up a daemon.

use crate::fleet::FleetConfig;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;

/// Minimal snapshot of an instance's reload-relevant fields. Anything used to
/// decide "did this instance change meaningfully for the running daemon" goes
/// here; anything that only matters at TUI-render time (display_name, etc.)
/// does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceDigest {
    pub backend_command: String,
    pub args: Vec<String>,
    pub role: Option<String>,
    pub topic_id: Option<i32>,
    pub working_directory: Option<PathBuf>,
}

impl InstanceDigest {
    /// Build from a resolved instance. Mirrors the fields `run_core` uses when
    /// registering `AgentConfig` for respawn — if any of these change in
    /// fleet.yaml, the live agent is out of sync with the file.
    pub fn from_config(config: &FleetConfig, name: &str) -> Option<Self> {
        let resolved = config.resolve_instance(name)?;
        Some(Self {
            backend_command: resolved.backend_command,
            args: resolved.args,
            role: resolved.role,
            topic_id: resolved.topic_id,
            working_directory: resolved.working_directory,
        })
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReloadDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub command_changed: Vec<String>,
    pub args_changed: Vec<String>,
    pub role_changed: Vec<String>,
    pub topic_id_changed: Vec<String>,
    pub working_dir_changed: Vec<String>,
}

impl ReloadDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.command_changed.is_empty()
            && self.args_changed.is_empty()
            && self.role_changed.is_empty()
            && self.topic_id_changed.is_empty()
            && self.working_dir_changed.is_empty()
    }
}

/// Pure-function diff between currently running agents (as recorded when last
/// reload tick ran, or at daemon startup) and the new fleet config.
pub fn compute_diff(
    current: &HashMap<String, InstanceDigest>,
    new: &HashMap<String, InstanceDigest>,
) -> ReloadDiff {
    let mut diff = ReloadDiff::default();
    let current_names: HashSet<&String> = current.keys().collect();
    let new_names: HashSet<&String> = new.keys().collect();

    for name in new_names.difference(&current_names) {
        diff.added.push((*name).clone());
    }
    for name in current_names.difference(&new_names) {
        diff.removed.push((*name).clone());
    }
    for name in new_names.intersection(&current_names) {
        let (Some(old), Some(nw)) = (current.get(*name), new.get(*name)) else {
            continue;
        };
        if old.backend_command != nw.backend_command {
            diff.command_changed.push((*name).clone());
        }
        if old.args != nw.args {
            diff.args_changed.push((*name).clone());
        }
        if old.role != nw.role {
            diff.role_changed.push((*name).clone());
        }
        if old.topic_id != nw.topic_id {
            diff.topic_id_changed.push((*name).clone());
        }
        if old.working_directory != nw.working_directory {
            diff.working_dir_changed.push((*name).clone());
        }
    }
    // Sort for stable logs + deterministic tests.
    diff.added.sort();
    diff.removed.sort();
    diff.command_changed.sort();
    diff.args_changed.sort();
    diff.role_changed.sort();
    diff.topic_id_changed.sort();
    diff.working_dir_changed.sort();
    diff
}

/// Build a digest map from a FleetConfig.
pub fn digest_from_config(config: &FleetConfig) -> HashMap<String, InstanceDigest> {
    config
        .instance_names()
        .into_iter()
        .filter_map(|name| InstanceDigest::from_config(config, &name).map(|d| (name, d)))
        .collect()
}

/// Polls fleet.yaml mtime; yields new `FleetConfig` on change.
///
/// Parse failures are logged once (mtime still advances) so we don't re-log
/// the same bad file every tick.
pub struct FleetWatcher {
    path: PathBuf,
    last_mtime: Option<SystemTime>,
}

impl FleetWatcher {
    pub fn new(path: PathBuf) -> Self {
        let last_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        Self { path, last_mtime }
    }

    /// Returns `Some(cfg)` when mtime advanced and the file parses cleanly.
    /// Returns `None` when the file is unchanged, missing, or unparseable.
    pub fn check(&mut self) -> Option<FleetConfig> {
        let meta = std::fs::metadata(&self.path).ok()?;
        let new_mtime = meta.modified().ok()?;
        if self.last_mtime == Some(new_mtime) {
            return None;
        }
        self.last_mtime = Some(new_mtime);
        match FleetConfig::load(&self.path) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                tracing::warn!(path = %self.path.display(), error = %e, "fleet reload parse failed");
                None
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-reload-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn d(cmd: &str, role: Option<&str>, topic: Option<i32>) -> InstanceDigest {
        InstanceDigest {
            backend_command: cmd.into(),
            args: vec![],
            role: role.map(Into::into),
            topic_id: topic,
            working_directory: None,
        }
    }

    #[test]
    fn diff_empty_when_equal() {
        let a = HashMap::from([("x".into(), d("bash", None, None))]);
        let b = a.clone();
        assert!(compute_diff(&a, &b).is_empty());
    }

    #[test]
    fn diff_detects_added() {
        let current = HashMap::new();
        let new = HashMap::from([("alice".into(), d("claude", None, None))]);
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.added, vec!["alice"]);
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn diff_detects_removed() {
        let current = HashMap::from([("bob".into(), d("claude", None, None))]);
        let new = HashMap::new();
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.removed, vec!["bob"]);
        assert!(diff.added.is_empty());
    }

    #[test]
    fn diff_detects_command_change() {
        let current = HashMap::from([("x".into(), d("bash", None, None))]);
        let new = HashMap::from([("x".into(), d("zsh", None, None))]);
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.command_changed, vec!["x"]);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn diff_detects_role_change() {
        let current = HashMap::from([("x".into(), d("bash", Some("old"), None))]);
        let new = HashMap::from([("x".into(), d("bash", Some("new"), None))]);
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.role_changed, vec!["x"]);
        assert!(diff.command_changed.is_empty());
    }

    #[test]
    fn diff_detects_topic_id_change() {
        let current = HashMap::from([("x".into(), d("bash", None, Some(1)))]);
        let new = HashMap::from([("x".into(), d("bash", None, Some(2)))]);
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.topic_id_changed, vec!["x"]);
    }

    #[test]
    fn diff_detects_args_change() {
        let mut old = d("bash", None, None);
        old.args = vec!["--foo".into()];
        let mut nw = d("bash", None, None);
        nw.args = vec!["--bar".into()];
        let current = HashMap::from([("x".into(), old)]);
        let new = HashMap::from([("x".into(), nw)]);
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.args_changed, vec!["x"]);
    }

    #[test]
    fn diff_detects_working_dir_change() {
        let mut old = d("bash", None, None);
        old.working_directory = Some(PathBuf::from("/a"));
        let mut nw = d("bash", None, None);
        nw.working_directory = Some(PathBuf::from("/b"));
        let current = HashMap::from([("x".into(), old)]);
        let new = HashMap::from([("x".into(), nw)]);
        let diff = compute_diff(&current, &new);
        assert_eq!(diff.working_dir_changed, vec!["x"]);
    }

    #[test]
    fn diff_detects_multiple_changes_sorted() {
        let current = HashMap::from([
            ("keep".into(), d("bash", None, None)),
            ("drop".into(), d("bash", None, None)),
            ("retitle".into(), d("bash", Some("a"), None)),
        ]);
        let new = HashMap::from([
            ("keep".into(), d("bash", None, None)),
            ("retitle".into(), d("bash", Some("b"), None)),
            ("zebra".into(), d("claude", None, None)),
            ("apple".into(), d("claude", None, None)),
        ]);
        let diff = compute_diff(&current, &new);
        // Sort stability
        assert_eq!(diff.added, vec!["apple", "zebra"]);
        assert_eq!(diff.removed, vec!["drop"]);
        assert_eq!(diff.role_changed, vec!["retitle"]);
        assert!(diff.command_changed.is_empty());
    }

    #[test]
    fn watcher_no_change_when_unmodified() {
        let dir = tmp_dir("w");
        let path = dir.join("fleet.yaml");
        std::fs::write(&path, "instances:\n  a:\n    command: bash\n").unwrap();
        let mut w = FleetWatcher::new(path.clone());
        assert!(
            w.check().is_none(),
            "first check after construction — mtime matches"
        );
    }

    #[test]
    fn watcher_yields_config_after_mtime_change() {
        let dir = tmp_dir("w");
        let path = dir.join("fleet.yaml");
        std::fs::write(&path, "instances:\n  a:\n    command: bash\n").unwrap();
        let mut w = FleetWatcher::new(path.clone());
        // Bump mtime by rewriting with different content.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(
            &path,
            "instances:\n  a:\n    command: bash\n  b:\n    command: zsh\n",
        )
        .unwrap();
        let cfg = w.check().expect("should detect change");
        assert_eq!(cfg.instances.len(), 2);
    }

    #[test]
    fn watcher_handles_parse_error() {
        let dir = tmp_dir("w");
        let path = dir.join("fleet.yaml");
        std::fs::write(&path, "instances:\n  a:\n    command: bash\n").unwrap();
        let mut w = FleetWatcher::new(path.clone());
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // instances must be a mapping, not a scalar
        std::fs::write(&path, "instances: not-a-map\n").unwrap();
        assert!(w.check().is_none(), "parse error yields None");
        // mtime is advanced — subsequent check with no further edits also None.
        assert!(w.check().is_none());
    }

    #[test]
    fn digest_from_config_roundtrip() {
        let yaml = "defaults:\n  backend: claude\ninstances:\n  alice:\n    role: researcher\n";
        let cfg: FleetConfig = serde_yaml::from_str(yaml).unwrap();
        let digest = digest_from_config(&cfg);
        assert_eq!(digest.len(), 1);
        assert!(digest["alice"].backend_command.contains("claude"));
        assert_eq!(digest["alice"].role.as_deref(), Some("researcher"));
    }
}
