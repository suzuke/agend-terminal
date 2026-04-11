//! Decision storage — CRUD over JSON files in {home}/decisions/.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub title: String,
    pub content: String,
    pub scope: String, // "project" or "fleet"
    pub author: String,
    pub tags: Vec<String>,
    pub ttl_days: Option<u64>,
    pub created_at: String,
    pub updated_at: String,
    pub archived: bool,
    pub supersedes: Option<String>,
    pub working_directory: Option<String>,
}

fn decisions_dir(home: &Path) -> std::path::PathBuf {
    home.join("decisions")
}

fn load_all(home: &Path) -> Vec<Decision> {
    let dir = decisions_dir(home);
    let mut decisions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().extension().and_then(|e| e.to_str()) == Some("json") {
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    if let Ok(d) = serde_json::from_str::<Decision>(&content) {
                        decisions.push(d);
                    }
                }
            }
        }
    }
    decisions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    decisions
}

fn save(home: &Path, decision: &Decision) -> anyhow::Result<()> {
    let dir = decisions_dir(home);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", decision.id));
    std::fs::write(&path, serde_json::to_string_pretty(decision)?)?;
    Ok(())
}

pub fn post(home: &Path, author: &str, args: &Value) -> Value {
    let title = match args["title"].as_str() {
        Some(t) => t,
        None => return serde_json::json!({"error": "missing 'title'"}),
    };
    let content = match args["content"].as_str() {
        Some(c) => c,
        None => return serde_json::json!({"error": "missing 'content'"}),
    };
    let scope = args["scope"].as_str().unwrap_or("project");
    let tags: Vec<String> = args["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let ttl_days = args["ttl_days"].as_u64();
    let supersedes = args["supersedes"].as_str().map(String::from);

    let now = chrono::Utc::now().to_rfc3339();
    let id = format!("d-{}", &now[..19].replace([':', '-', 'T'], ""));

    // If superseding, archive old decision
    if let Some(ref old_id) = supersedes {
        let mut all = load_all(home);
        if let Some(old) = all.iter_mut().find(|d| d.id == *old_id) {
            old.archived = true;
            old.updated_at = now.clone();
            let _ = save(home, old);
        }
    }

    let working_dir = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());

    let decision = Decision {
        id: id.clone(),
        title: title.to_string(),
        content: content.to_string(),
        scope: scope.to_string(),
        author: author.to_string(),
        tags,
        ttl_days,
        created_at: now.clone(),
        updated_at: now,
        archived: false,
        supersedes,
        working_directory: working_dir,
    };

    match save(home, &decision) {
        Ok(()) => serde_json::json!({"id": id, "status": "posted"}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

pub fn list(home: &Path, args: &Value) -> Value {
    let include_archived = args["include_archived"].as_bool().unwrap_or(false);
    let filter_tags: Vec<String> = args["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let all = load_all(home);
    let filtered: Vec<_> = all
        .into_iter()
        .filter(|d| include_archived || !d.archived)
        .filter(|d| {
            filter_tags.is_empty() || filter_tags.iter().any(|t| d.tags.contains(t))
        })
        .collect();

    serde_json::json!({"decisions": filtered})
}

pub fn update(home: &Path, args: &Value) -> Value {
    let id = match args["id"].as_str() {
        Some(i) => i,
        None => return serde_json::json!({"error": "missing 'id'"}),
    };

    let mut all = load_all(home);
    let decision = match all.iter_mut().find(|d| d.id == id) {
        Some(d) => d,
        None => return serde_json::json!({"error": format!("decision '{id}' not found")}),
    };

    if let Some(content) = args["content"].as_str() {
        decision.content = content.to_string();
    }
    if let Some(tags) = args["tags"].as_array() {
        decision.tags = tags.iter().filter_map(|v| v.as_str().map(String::from)).collect();
    }
    if let Some(ttl) = args["ttl_days"].as_u64() {
        decision.ttl_days = Some(ttl);
    }
    if args["archive"].as_bool() == Some(true) {
        decision.archived = true;
    }
    decision.updated_at = chrono::Utc::now().to_rfc3339();

    let decision = decision.clone();
    match save(home, &decision) {
        Ok(()) => serde_json::json!({"id": id, "status": "updated"}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}
