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

pub fn create(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n.to_string(),
        None => return serde_json::json!({"error": "missing 'name'"}),
    };
    let members: Vec<String> = match args["members"].as_array() {
        Some(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => return serde_json::json!({"error": "missing 'members'"}),
    };
    let description = args["description"].as_str().map(String::from);
    match crate::store::mutate(&store_path(home), |store: &mut TeamStore| {
        if store.teams.iter().any(|t| t.name == name) {
            return Ok(false);
        }
        store.teams.push(Team {
            name: name.clone(),
            members,
            description,
            created_at: chrono::Utc::now().to_rfc3339(),
        });
        Ok(true)
    }) {
        Ok(true) => serde_json::json!({"status": "created", "name": name}),
        Ok(false) => serde_json::json!({"error": format!("team '{name}' already exists")}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

pub fn delete(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n.to_string(),
        None => return serde_json::json!({"error": "missing 'name'"}),
    };
    match crate::store::mutate(&store_path(home), |store: &mut TeamStore| {
        let before = store.teams.len();
        store.teams.retain(|t| t.name != name);
        Ok(store.teams.len() < before)
    }) {
        Ok(true) => serde_json::json!({"status": "deleted", "name": name}),
        Ok(false) => serde_json::json!({"error": format!("team '{name}' not found")}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

pub fn list(home: &Path) -> Value {
    let store = load(home);
    serde_json::json!({"teams": store.teams})
}

pub fn update(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n.to_string(),
        None => return serde_json::json!({"error": "missing 'name'"}),
    };
    let to_add: Vec<String> = args["add"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let to_remove: Vec<String> = args["remove"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    match crate::store::mutate(&store_path(home), |store: &mut TeamStore| {
        match store.teams.iter_mut().find(|t| t.name == name) {
            Some(team) => {
                for m in &to_add {
                    if !team.members.contains(m) {
                        team.members.push(m.clone());
                    }
                }
                team.members.retain(|m| !to_remove.contains(m));
                Ok(true)
            }
            None => Ok(false),
        }
    }) {
        Ok(true) => serde_json::json!({"status": "updated", "name": name}),
        Ok(false) => serde_json::json!({"error": format!("team '{name}' not found")}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-teams-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn test_create_list_update_delete() {
        let home = tmp_home("crud");
        let r = create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["a", "b"]}),
        );
        assert_eq!(r["status"], "created");

        let listed = list(&home);
        assert_eq!(listed["teams"].as_array().expect("arr").len(), 1);
        assert_eq!(
            listed["teams"][0]["members"].as_array().expect("m").len(),
            2
        );

        // Add member
        update(&home, &serde_json::json!({"name": "devs", "add": ["c"]}));
        let members = get_members(&home, "devs");
        assert_eq!(members, vec!["a", "b", "c"]);

        // Remove member
        update(&home, &serde_json::json!({"name": "devs", "remove": ["a"]}));
        let members = get_members(&home, "devs");
        assert_eq!(members, vec!["b", "c"]);

        // Duplicate add ignored
        update(&home, &serde_json::json!({"name": "devs", "add": ["b"]}));
        let members = get_members(&home, "devs");
        assert_eq!(members, vec!["b", "c"]);

        // Delete
        let r = delete(&home, &serde_json::json!({"name": "devs"}));
        assert_eq!(r["status"], "deleted");
        assert!(list(&home)["teams"].as_array().expect("arr").is_empty());

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_duplicate_create() {
        let home = tmp_home("dup_create");
        create(&home, &serde_json::json!({"name": "t", "members": ["a"]}));
        let r = create(&home, &serde_json::json!({"name": "t", "members": ["b"]}));
        assert!(r["error"].as_str().expect("err").contains("already exists"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_delete_nonexistent() {
        let home = tmp_home("del_nonexistent");
        let r = delete(&home, &serde_json::json!({"name": "nope"}));
        assert!(r["error"].as_str().is_some());
        std::fs::remove_dir_all(&home).ok();
    }
}

/// Get members of a team.
pub fn get_members(home: &Path, team_name: &str) -> Vec<String> {
    let store = load(home);
    store
        .teams
        .iter()
        .find(|t| t.name == team_name)
        .map(|t| t.members.clone())
        .unwrap_or_default()
}
