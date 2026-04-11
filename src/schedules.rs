//! Schedule storage — CRUD for cron schedules. Execution via daemon::check_schedules().

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub cron: String,
    pub message: String,
    pub target: String,
    pub label: Option<String>,
    pub timezone: String,
    pub enabled: bool,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub run_history: Vec<ScheduleRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleRun {
    pub triggered_at: String,
    pub status: String, // "ok" or error message
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ScheduleStore {
    schedules: Vec<Schedule>,
}

fn store_path(home: &Path) -> std::path::PathBuf {
    crate::store::store_path(home, "schedules.json")
}

fn load(home: &Path) -> ScheduleStore {
    crate::store::load(&store_path(home))
}

fn save(home: &Path, store: &ScheduleStore) -> anyhow::Result<()> {
    crate::store::save(&store_path(home), store)
}

pub fn create(home: &Path, instance_name: &str, args: &Value) -> Value {
    let cron = match args["cron"].as_str() {
        Some(c) => c,
        None => return serde_json::json!({"error": "missing 'cron'"}),
    };
    let message = match args["message"].as_str() {
        Some(m) => m,
        None => return serde_json::json!({"error": "missing 'message'"}),
    };
    let now = chrono::Utc::now().to_rfc3339();
    let id = format!("s-{}", &now[..19].replace([':', '-', 'T'], ""));
    let schedule = Schedule {
        id: id.clone(),
        cron: cron.to_string(),
        message: message.to_string(),
        target: args["target"].as_str().unwrap_or(instance_name).to_string(),
        label: args["label"].as_str().map(String::from),
        timezone: args["timezone"].as_str().unwrap_or("Asia/Taipei").to_string(),
        enabled: true,
        created_by: instance_name.to_string(),
        created_at: now.clone(),
        updated_at: now,
        run_history: Vec::new(),
    };
    let mut store = load(home);
    store.schedules.push(schedule);
    match save(home, &store) {
        Ok(()) => serde_json::json!({"id": id, "status": "created"}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

pub fn list(home: &Path, args: &Value) -> Value {
    let store = load(home);
    let target_filter = args["target"].as_str();
    let filtered: Vec<_> = store.schedules.iter()
        .filter(|s| target_filter.map_or(true, |t| s.target == t))
        .collect();
    serde_json::json!({"schedules": filtered})
}

pub fn update(home: &Path, args: &Value) -> Value {
    let id = match args["id"].as_str() {
        Some(i) => i,
        None => return serde_json::json!({"error": "missing 'id'"}),
    };
    let mut store = load(home);
    match store.schedules.iter_mut().find(|s| s.id == id) {
        Some(schedule) => {
            if let Some(c) = args["cron"].as_str() { schedule.cron = c.to_string(); }
            if let Some(m) = args["message"].as_str() { schedule.message = m.to_string(); }
            if let Some(t) = args["target"].as_str() { schedule.target = t.to_string(); }
            if let Some(l) = args["label"].as_str() { schedule.label = Some(l.to_string()); }
            if let Some(tz) = args["timezone"].as_str() { schedule.timezone = tz.to_string(); }
            if let Some(e) = args["enabled"].as_bool() { schedule.enabled = e; }
            schedule.updated_at = chrono::Utc::now().to_rfc3339();
            let _ = save(home, &store);
            serde_json::json!({"id": id, "status": "updated"})
        }
        None => serde_json::json!({"error": format!("schedule '{id}' not found")}),
    }
}

/// Record a schedule execution result. Called by daemon after cron trigger.
pub fn record_run(home: &Path, schedule_id: &str, status: &str) {
    let mut store = load(home);
    if let Some(sched) = store.schedules.iter_mut().find(|s| s.id == schedule_id) {
        sched.run_history.push(ScheduleRun {
            triggered_at: chrono::Utc::now().to_rfc3339(),
            status: status.to_string(),
        });
        // Keep last 50 runs only
        if sched.run_history.len() > 50 {
            let excess = sched.run_history.len() - 50;
            sched.run_history.drain(..excess);
        }
        let _ = save(home, &store);
    }
}

pub fn delete(home: &Path, args: &Value) -> Value {
    let id = match args["id"].as_str() {
        Some(i) => i,
        None => return serde_json::json!({"error": "missing 'id'"}),
    };
    let mut store = load(home);
    let before = store.schedules.len();
    store.schedules.retain(|s| s.id != id);
    if store.schedules.len() == before {
        return serde_json::json!({"error": format!("schedule '{id}' not found")});
    }
    match save(home, &store) {
        Ok(()) => serde_json::json!({"id": id, "status": "deleted"}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}
