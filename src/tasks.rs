//! Task board — fleet-wide task tracking via JSON file.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: String, // open, claimed, done, blocked, cancelled
    pub priority: String, // low, normal, high, urgent
    pub assignee: Option<String>,
    pub created_by: String,
    pub depends_on: Vec<String>,
    pub result: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TaskStore {
    tasks: Vec<Task>,
}

fn store_path(home: &Path) -> std::path::PathBuf {
    home.join("tasks.json")
}

fn load(home: &Path) -> TaskStore {
    let path = store_path(home);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

fn save(home: &Path, store: &TaskStore) -> anyhow::Result<()> {
    let path = store_path(home);
    std::fs::write(&path, serde_json::to_string_pretty(store)?)?;
    Ok(())
}

pub fn handle(home: &Path, instance_name: &str, args: &Value) -> Value {
    let action = match args["action"].as_str() {
        Some(a) => a,
        None => return serde_json::json!({"error": "missing 'action'"}),
    };

    match action {
        "create" => {
            let title = match args["title"].as_str() {
                Some(t) => t,
                None => return serde_json::json!({"error": "missing 'title'"}),
            };
            let now = chrono::Utc::now().to_rfc3339();
            let id = format!("t-{}", &now[..19].replace([':', '-', 'T'], ""));
            let task = Task {
                id: id.clone(),
                title: title.to_string(),
                description: args["description"].as_str().unwrap_or("").to_string(),
                status: "open".to_string(),
                priority: args["priority"].as_str().unwrap_or("normal").to_string(),
                assignee: args["assignee"].as_str().map(String::from),
                created_by: instance_name.to_string(),
                depends_on: args["depends_on"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default(),
                result: None,
                created_at: now.clone(),
                updated_at: now,
            };
            let mut store = load(home);
            store.tasks.push(task);
            match save(home, &store) {
                Ok(()) => serde_json::json!({"id": id, "status": "created"}),
                Err(e) => serde_json::json!({"error": format!("{e}")}),
            }
        }
        "list" => {
            let store = load(home);
            let filter_assignee = args["filter_assignee"].as_str();
            let filter_status = args["filter_status"].as_str();
            let filtered: Vec<_> = store.tasks.iter()
                .filter(|t| filter_assignee.map_or(true, |a| t.assignee.as_deref() == Some(a)))
                .filter(|t| filter_status.map_or(true, |s| t.status == s))
                .collect();
            serde_json::json!({"tasks": filtered})
        }
        "claim" => {
            let id = match args["id"].as_str() {
                Some(i) => i,
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            let mut store = load(home);
            match store.tasks.iter_mut().find(|t| t.id == id) {
                Some(task) => {
                    task.status = "claimed".to_string();
                    task.assignee = Some(instance_name.to_string());
                    task.updated_at = chrono::Utc::now().to_rfc3339();
                    let _ = save(home, &store);
                    serde_json::json!({"id": id, "status": "claimed", "assignee": instance_name})
                }
                None => serde_json::json!({"error": format!("task '{id}' not found")}),
            }
        }
        "done" => {
            let id = match args["id"].as_str() {
                Some(i) => i,
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            let mut store = load(home);
            match store.tasks.iter_mut().find(|t| t.id == id) {
                Some(task) => {
                    task.status = "done".to_string();
                    task.result = args["result"].as_str().map(String::from);
                    task.updated_at = chrono::Utc::now().to_rfc3339();
                    let _ = save(home, &store);
                    serde_json::json!({"id": id, "status": "done"})
                }
                None => serde_json::json!({"error": format!("task '{id}' not found")}),
            }
        }
        "update" => {
            let id = match args["id"].as_str() {
                Some(i) => i,
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            let mut store = load(home);
            match store.tasks.iter_mut().find(|t| t.id == id) {
                Some(task) => {
                    if let Some(s) = args["status"].as_str() { task.status = s.to_string(); }
                    if let Some(p) = args["priority"].as_str() { task.priority = p.to_string(); }
                    if let Some(a) = args["assignee"].as_str() { task.assignee = Some(a.to_string()); }
                    task.updated_at = chrono::Utc::now().to_rfc3339();
                    let _ = save(home, &store);
                    serde_json::json!({"id": id, "status": "updated"})
                }
                None => serde_json::json!({"error": format!("task '{id}' not found")}),
            }
        }
        _ => serde_json::json!({"error": format!("unknown action: {action}")}),
    }
}
