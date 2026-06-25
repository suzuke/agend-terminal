//! Fleet snapshot: periodic state persistence for daemon restart awareness.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize)]
pub struct FleetSnapshot {
    pub timestamp: String,
    pub agents: Vec<AgentSnapshot>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct AgentSnapshot {
    pub name: String,
    pub backend_command: String,
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    pub submit_key: String,
    pub health_state: String,
    pub agent_state: String,
    /// #1694②: seconds since the agent last produced *productive* output
    /// (`StateTracker::last_productive_output`, marker/heartbeat-gated — resists
    /// spinner/blink/junk). The dispatch-idle silence-clock reads this to gate on
    /// "is the agent actually making progress" rather than the instantaneous
    /// thinking/tool_use state. `serde(default)` returns a large value so a
    /// snapshot written by an older daemon (no field) fails OPEN — the watchdog
    /// fires rather than silently suppressing a possible stuck.
    #[serde(default = "default_silent_secs")]
    pub silent_secs: i64,
    /// #1961 phase-2: seconds since the pane CONTENT last changed (raw screen
    /// hash delta, `StateTracker::output_silence` — classification-free). The
    /// dispatch-idle suppress reads this as its state-detector-independent
    /// activity signal: a streaming/working pane keeps changing even when the
    /// detector mis-classifies the agent as idle (the #1961 false-fire).
    /// `serde(default)` fails OPEN (huge value = "no recent change" → fire).
    #[serde(default = "default_silent_secs")]
    pub output_silent_secs: i64,
}

/// Fail-open default for a snapshot missing `silent_secs` (old-format / boot
/// transient): a large value reads as "very silent" → the idle watchdog fires
/// rather than suppressing. The live daemon rewrites the snapshot every tick with
/// the real value, so this only covers the sub-tick boot window.
fn default_silent_secs() -> i64 {
    i64::MAX
}

pub fn save(home: &Path, agents: &[AgentSnapshot]) {
    let snapshot = FleetSnapshot {
        timestamp: chrono::Utc::now().to_rfc3339(),
        agents: agents.to_vec(),
    };
    let path = home.join("snapshot.json");
    if let Err(e) = crate::store::save_atomic(&path, &snapshot) {
        tracing::warn!(path = %path.display(), error = %e, "failed to persist snapshot");
    }
}

pub fn load(home: &Path) -> Option<FleetSnapshot> {
    let path = home.join("snapshot.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
}

/// #1513: an agent's current state string from the latest per-tick snapshot.
/// LOCK-FREE (file read) — safe on the inject path, which must NEVER take the
/// per-agent core lock (#1492 self-IPC-under-lock deadlock class).
pub fn agent_state_of(home: &Path, agent_name: &str) -> Option<String> {
    load(home)?
        .agents
        .into_iter()
        .find(|a| a.name == agent_name)
        .map(|a| a.agent_state)
}

/// #1513: is the agent mid-generation (`Active`) per the latest
/// snapshot? Fail-OPEN (returns false on a missing snapshot/agent) so a stale or
/// absent snapshot never starves the notification queue — the MAX_DEFER cap is
/// the backstop. Matches `AgentState::display_name` (`active`).
pub fn agent_is_busy(home: &Path, agent_name: &str) -> bool {
    agent_state_of(home, agent_name)
        .map(|s| s.as_str() == "active")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp_home(suffix: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agend-snap-{}-{}", suffix, std::process::id()));
        fs::create_dir_all(&dir).ok();
        dir
    }

    fn make_agent(name: &str, state: &str) -> AgentSnapshot {
        AgentSnapshot {
            name: name.to_string(),
            backend_command: "claude".to_string(),
            args: vec!["--dangerously-skip-permissions".to_string()],
            working_dir: Some("/tmp/work".to_string()),
            submit_key: "\r".to_string(),
            health_state: "healthy".to_string(),
            agent_state: state.to_string(),
            silent_secs: 0,
            output_silent_secs: 0,
        }
    }

    #[test]
    fn save_load_roundtrip() {
        let home = tmp_home("roundtrip");
        let agents = vec![make_agent("agent1", "idle"), make_agent("agent2", "busy")];
        save(&home, &agents);

        let snapshot = load(&home).expect("should load");
        assert_eq!(snapshot.agents.len(), 2);
        assert_eq!(snapshot.agents[0].name, "agent1");
        assert_eq!(snapshot.agents[1].name, "agent2");
        assert!(!snapshot.timestamp.is_empty());

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn load_missing_file_returns_none() {
        let home = tmp_home("missing");
        let result = load(&home);
        assert!(result.is_none());
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn agent_state_preserved() {
        let home = tmp_home("state");
        let agents = vec![make_agent("a1", "working")];
        save(&home, &agents);

        let snapshot = load(&home).expect("should load");
        let a = &snapshot.agents[0];
        assert_eq!(a.agent_state, "working");
        assert_eq!(a.health_state, "healthy");
        assert_eq!(a.backend_command, "claude");
        assert_eq!(a.args, vec!["--dangerously-skip-permissions"]);
        assert_eq!(a.working_dir.as_deref(), Some("/tmp/work"));
        assert_eq!(a.submit_key, "\r");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_overwrites_previous() {
        let home = tmp_home("overwrite");
        save(&home, &[make_agent("first", "idle")]);
        save(
            &home,
            &[make_agent("second", "busy"), make_agent("third", "idle")],
        );

        let snapshot = load(&home).expect("should load");
        assert_eq!(snapshot.agents.len(), 2);
        assert_eq!(snapshot.agents[0].name, "second");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn empty_agents_snapshot() {
        let home = tmp_home("empty");
        save(&home, &[]);

        let snapshot = load(&home).expect("should load");
        assert!(snapshot.agents.is_empty());

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn snapshot_json_is_pretty_printed() {
        let home = tmp_home("pretty");
        save(&home, &[make_agent("x", "idle")]);

        let content = fs::read_to_string(home.join("snapshot.json")).expect("read");
        // Pretty-printed JSON has newlines
        assert!(content.contains('\n'), "should be pretty-printed");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn agent_snapshot_serialization() {
        let agent = make_agent("test", "running");
        let json = serde_json::to_string(&agent).expect("serialize");
        let parsed: AgentSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.name, "test");
        assert_eq!(parsed.agent_state, "running");
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let home = tmp_home("atomic");
        save(&home, &[make_agent("a", "idle")]);
        let tmp = home.join("snapshot.json.tmp");
        assert!(
            !tmp.exists(),
            "temporary file must be renamed, not left behind"
        );
        assert!(home.join("snapshot.json").exists());
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_overwrites_atomically_without_truncating_to_zero() {
        // If a reader observes snapshot.json between truncate and write, it
        // would see an empty file. Atomic rename must prevent that window.
        let home = tmp_home("atomic-size");
        save(&home, &[make_agent("first", "idle")]);
        let first_size = fs::metadata(home.join("snapshot.json"))
            .expect("first meta")
            .len();
        save(
            &home,
            &[make_agent("second", "busy"), make_agent("third", "idle")],
        );
        let second_size = fs::metadata(home.join("snapshot.json"))
            .expect("second meta")
            .len();
        assert!(first_size > 0 && second_size > 0);
        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_snapshot_timestamp_is_rfc3339() {
        let home = tmp_home("ts");
        save(&home, &[]);
        let snapshot = load(&home).expect("load");
        // Should parse as a valid datetime
        assert!(
            chrono::DateTime::parse_from_rfc3339(&snapshot.timestamp).is_ok(),
            "timestamp should be valid RFC3339"
        );
        fs::remove_dir_all(&home).ok();
    }
}
