//! Schedule storage — CRUD for cron schedules. Execution via daemon::check_schedules().

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::str::FromStr;

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
    let full_expr = if cron.split_whitespace().count() == 5 {
        format!("0 {cron}")
    } else {
        cron.to_string()
    };
    if cron::Schedule::from_str(&full_expr).is_err() {
        return serde_json::json!({"error": format!("invalid cron expression: {cron}")});
    }
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
        timezone: args["timezone"]
            .as_str()
            .unwrap_or("Asia/Taipei")
            .to_string(),
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
    let filtered: Vec<_> = store
        .schedules
        .iter()
        .filter(|s| target_filter.is_none_or(|t| s.target == t))
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
            if let Some(c) = args["cron"].as_str() {
                schedule.cron = c.to_string();
            }
            if let Some(m) = args["message"].as_str() {
                schedule.message = m.to_string();
            }
            if let Some(t) = args["target"].as_str() {
                schedule.target = t.to_string();
            }
            if let Some(l) = args["label"].as_str() {
                schedule.label = Some(l.to_string());
            }
            if let Some(tz) = args["timezone"].as_str() {
                schedule.timezone = tz.to_string();
            }
            if let Some(e) = args["enabled"].as_bool() {
                schedule.enabled = e;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-schedules-test-{}-{}-{}",
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
            "agent1",
            &serde_json::json!({"cron": "0 9 * * *", "message": "hello", "label": "morning"}),
        );
        assert_eq!(r["status"], "created");
        let id = r["id"].as_str().expect("id").to_string();

        let listed = list(&home, &serde_json::json!({}));
        assert_eq!(listed["schedules"].as_array().expect("arr").len(), 1);
        assert_eq!(listed["schedules"][0]["label"], "morning");

        // Update
        update(&home, &serde_json::json!({"id": id, "enabled": false}));
        let listed = list(&home, &serde_json::json!({}));
        assert_eq!(listed["schedules"][0]["enabled"], false);

        // Delete
        let r = delete(&home, &serde_json::json!({"id": id}));
        assert_eq!(r["status"], "deleted");
        assert!(list(&home, &serde_json::json!({}))["schedules"]
            .as_array()
            .expect("arr")
            .is_empty());

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_run_history() {
        let home = tmp_home("run_history");
        let r = create(
            &home,
            "a",
            &serde_json::json!({"cron": "* * * * *", "message": "test"}),
        );
        let id = r["id"].as_str().expect("id").to_string();

        record_run(&home, &id, "ok");
        record_run(&home, &id, "ok");
        record_run(&home, &id, "inject_failed");

        let listed = list(&home, &serde_json::json!({}));
        let history = listed["schedules"][0]["run_history"]
            .as_array()
            .expect("arr");
        assert_eq!(history.len(), 3);
        assert_eq!(history[2]["status"], "inject_failed");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_filter_by_target() {
        let home = tmp_home("filter_target");
        create(
            &home,
            "a",
            &serde_json::json!({"cron": "0 9 * * *", "message": "m1", "target": "agent1"}),
        );
        create(
            &home,
            "a",
            &serde_json::json!({"cron": "0 10 * * *", "message": "m2", "target": "agent2"}),
        );

        let listed = list(&home, &serde_json::json!({"target": "agent1"}));
        assert_eq!(listed["schedules"].as_array().expect("arr").len(), 1);

        std::fs::remove_dir_all(&home).ok();
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
