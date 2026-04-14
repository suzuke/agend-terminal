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
}

pub fn save(home: &Path, agents: &[AgentSnapshot]) {
    let snapshot = FleetSnapshot {
        timestamp: chrono::Utc::now().to_rfc3339(),
        agents: agents.to_vec(),
    };
    let path = home.join("snapshot.json");
    let _ = std::fs::write(
        &path,
        serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
    );
}

pub fn load(home: &Path) -> Option<FleetSnapshot> {
    let path = home.join("snapshot.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
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
