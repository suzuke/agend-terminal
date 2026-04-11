//! Team management — named groups of instances for broadcast targeting.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub name: String,
    pub members: Vec<String>,
    pub description: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TeamStore {
    teams: Vec<Team>,
}

fn store_path(home: &Path) -> std::path::PathBuf {
    crate::store::store_path(home, "teams.json")
}

fn load(home: &Path) -> TeamStore {
    crate::store::load(&store_path(home))
}

fn save(home: &Path, store: &TeamStore) -> anyhow::Result<()> {
    crate::store::save(&store_path(home), store)
}

pub fn create(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return serde_json::json!({"error": "missing 'name'"}),
    };
    let members: Vec<String> = match args["members"].as_array() {
        Some(a) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        None => return serde_json::json!({"error": "missing 'members'"}),
    };
    let mut store = load(home);
    if store.teams.iter().any(|t| t.name == name) {
        return serde_json::json!({"error": format!("team '{name}' already exists")});
    }
    store.teams.push(Team {
        name: name.to_string(),
        members,
        description: args["description"].as_str().map(String::from),
        created_at: chrono::Utc::now().to_rfc3339(),
    });
    match save(home, &store) {
        Ok(()) => serde_json::json!({"status": "created", "name": name}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

pub fn delete(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return serde_json::json!({"error": "missing 'name'"}),
    };
    let mut store = load(home);
    let before = store.teams.len();
    store.teams.retain(|t| t.name != name);
    if store.teams.len() == before {
        return serde_json::json!({"error": format!("team '{name}' not found")});
    }
    match save(home, &store) {
        Ok(()) => serde_json::json!({"status": "deleted", "name": name}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

pub fn list(home: &Path) -> Value {
    let store = load(home);
    serde_json::json!({"teams": store.teams})
}

pub fn update(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return serde_json::json!({"error": "missing 'name'"}),
    };
    let mut store = load(home);
    match store.teams.iter_mut().find(|t| t.name == name) {
        Some(team) => {
            if let Some(add) = args["add"].as_array() {
                for m in add.iter().filter_map(|v| v.as_str()) {
                    if !team.members.contains(&m.to_string()) {
                        team.members.push(m.to_string());
                    }
                }
            }
            if let Some(remove) = args["remove"].as_array() {
                let to_remove: Vec<String> = remove.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                team.members.retain(|m| !to_remove.contains(m));
            }
            let _ = save(home, &store);
            serde_json::json!({"status": "updated", "name": name})
        }
        None => serde_json::json!({"error": format!("team '{name}' not found")}),
    }
}

/// Get members of a team.
pub fn get_members(home: &Path, team_name: &str) -> Vec<String> {
    let store = load(home);
    store.teams.iter()
        .find(|t| t.name == team_name)
        .map(|t| t.members.clone())
        .unwrap_or_default()
}
