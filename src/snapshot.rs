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
    pub command: String,
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
