//! Task board — fleet-wide task tracking via JSON file.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: String,   // open, claimed, done, blocked, cancelled
    pub priority: String, // low, normal, high, urgent
    pub assignee: Option<String>,
    /// When assignee is a team name, this holds the resolved orchestrator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed_to: Option<String>,
    pub created_by: String,
    pub depends_on: Vec<String>,
    pub result: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TaskStore {
    #[serde(default)]
    schema_version: u32,
    tasks: Vec<Task>,
}

impl crate::store::SchemaVersioned for TaskStore {
    const CURRENT: u32 = 1;
    fn version_mut(&mut self) -> &mut u32 {
        &mut self.schema_version
    }
}

fn store_path(home: &Path) -> std::path::PathBuf {
    crate::store::store_path(home, "tasks.json")
}

fn load(home: &Path) -> TaskStore {
    crate::store::load_versioned(
        &store_path(home),
        <TaskStore as crate::store::SchemaVersioned>::CURRENT,
    )
}

/// Return all tasks as typed structs (no JSON round-trip).
pub fn list_all(home: &Path) -> Vec<Task> {
    load(home).tasks
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
            let assignee = args["assignee"].as_str().map(String::from);
            // Resolve team → orchestrator routing
            let routed_to = if let Some(ref name) = assignee {
                match crate::teams::resolve_team_orchestrator(home, name) {
                    Ok(Some(orch)) => Some(orch),
                    Ok(None) => None, // not a team, direct assignment
                    Err(e) => return serde_json::json!({"error": e}),
                }
            } else {
                None
            };
            let task = Task {
                id: id.clone(),
                title: title.to_string(),
                description: args["description"].as_str().unwrap_or("").to_string(),
                status: "open".to_string(),
                priority: args["priority"].as_str().unwrap_or("normal").to_string(),
                assignee,
                routed_to,
                created_by: instance_name.to_string(),
                depends_on: args["depends_on"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                result: None,
                created_at: now.clone(),
                updated_at: now,
            };
            match crate::store::mutate_versioned(&store_path(home), |store: &mut TaskStore| {
                store.tasks.push(task);
                Ok(())
            }) {
                Ok(()) => serde_json::json!({"id": id, "status": "created"}),
                Err(e) => serde_json::json!({"error": format!("{e}")}),
            }
        }
        "list" => {
            let store = load(home);
            let filter_assignee = args["filter_assignee"].as_str();
            let filter_status = args["filter_status"].as_str();
            let filtered: Vec<_> = store
                .tasks
                .iter()
                .filter(|t| filter_assignee.is_none_or(|a| t.assignee.as_deref() == Some(a)))
                .filter(|t| filter_status.is_none_or(|s| t.status == s))
                .collect();
            serde_json::json!({"tasks": filtered})
        }
        "claim" => {
            let id = match args["id"].as_str() {
                Some(i) => i.to_string(),
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            let iname = instance_name.to_string();
            match crate::store::mutate_versioned(&store_path(home), |store: &mut TaskStore| {
                match store.tasks.iter_mut().find(|t| t.id == id) {
                    Some(task) => {
                        task.status = "claimed".to_string();
                        task.assignee = Some(iname.clone());
                        task.routed_to = None; // agent claims directly
                        task.updated_at = chrono::Utc::now().to_rfc3339();
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }) {
                Ok(true) => {
                    serde_json::json!({"id": id, "status": "claimed", "assignee": instance_name})
                }
                Ok(false) => serde_json::json!({"error": format!("task '{id}' not found")}),
                Err(e) => serde_json::json!({"error": format!("{e}")}),
            }
        }
        "done" => {
            let id = match args["id"].as_str() {
                Some(i) => i.to_string(),
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            let result_text = args["result"].as_str().map(String::from);
            match crate::store::mutate_versioned(&store_path(home), |store: &mut TaskStore| {
                match store.tasks.iter_mut().find(|t| t.id == id) {
                    Some(task) => {
                        task.status = "done".to_string();
                        task.result.clone_from(&result_text);
                        task.updated_at = chrono::Utc::now().to_rfc3339();
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }) {
                Ok(true) => serde_json::json!({"id": id, "status": "done"}),
                Ok(false) => serde_json::json!({"error": format!("task '{id}' not found")}),
                Err(e) => serde_json::json!({"error": format!("{e}")}),
            }
        }
        "update" => {
            let id = match args["id"].as_str() {
                Some(i) => i.to_string(),
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            let new_status = args["status"].as_str().map(String::from);
            let new_priority = args["priority"].as_str().map(String::from);
            let new_assignee = args["assignee"].as_str().map(String::from);
            // Resolve team routing for new assignee
            let new_routed_to = if let Some(ref name) = new_assignee {
                match crate::teams::resolve_team_orchestrator(home, name) {
                    Ok(orch) => orch, // Some(orch) for team, None for agent
                    Err(e) => return serde_json::json!({"error": e}),
                }
            } else {
                None
            };
            match crate::store::mutate_versioned(&store_path(home), |store: &mut TaskStore| {
                match store.tasks.iter_mut().find(|t| t.id == id) {
                    Some(task) => {
                        if let Some(ref s) = new_status {
                            task.status = s.clone();
                        }
                        if let Some(ref p) = new_priority {
                            task.priority = p.clone();
                        }
                        if let Some(ref a) = new_assignee {
                            task.assignee = Some(a.clone());
                            task.routed_to = new_routed_to.clone();
                        }
                        task.updated_at = chrono::Utc::now().to_rfc3339();
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }) {
                Ok(true) => serde_json::json!({"id": id, "status": "updated"}),
                Ok(false) => serde_json::json!({"error": format!("task '{id}' not found")}),
                Err(e) => serde_json::json!({"error": format!("{e}")}),
            }
        }
        _ => serde_json::json!({"error": format!("unknown action: {action}")}),
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
            "agend-tasks-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn test_create_list_claim_done() {
        let home = tmp_home("crud");
        let r = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "Fix bug", "priority": "high"}),
        );
        assert_eq!(r["status"], "created");
        let id = r["id"].as_str().expect("id").to_string();

        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        assert_eq!(listed["tasks"].as_array().expect("arr").len(), 1);
        assert_eq!(listed["tasks"][0]["status"], "open");

        let claim = handle(
            &home,
            "agent2",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        assert_eq!(claim["status"], "claimed");
        assert_eq!(claim["assignee"], "agent2");

        let done = handle(
            &home,
            "agent2",
            &serde_json::json!({"action": "done", "id": id, "result": "fixed"}),
        );
        assert_eq!(done["status"], "done");

        let listed = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "list", "filter_status": "done"}),
        );
        assert_eq!(listed["tasks"][0]["result"], "fixed");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_claim_nonexistent() {
        let home = tmp_home("claim_nonexistent");
        let r = handle(
            &home,
            "a",
            &serde_json::json!({"action": "claim", "id": "nope"}),
        );
        assert!(r["error"].as_str().is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_assign_to_team_routes_to_orchestrator() {
        let home = tmp_home("team_route");
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        let r = handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "fix bug", "assignee": "devs"}),
        );
        assert_eq!(r["status"], "created");
        let tasks = list_all(&home);
        let t = tasks.iter().find(|t| t.title == "fix bug").expect("task");
        assert_eq!(t.assignee.as_deref(), Some("devs"));
        assert_eq!(t.routed_to.as_deref(), Some("lead"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_assign_to_degraded_team_rejects() {
        let home = tmp_home("degraded_reject");
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        crate::teams::remove_member_from_all(&home, "lead");
        let r = handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "fix bug", "assignee": "devs"}),
        );
        assert!(
            r["error"].as_str().expect("err").contains("degraded"),
            "got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_assign_to_agent_unchanged() {
        let home = tmp_home("agent_direct");
        let r = handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "fix bug", "assignee": "at-dev-2"}),
        );
        assert_eq!(r["status"], "created");
        let tasks = list_all(&home);
        let t = tasks.iter().find(|t| t.title == "fix bug").expect("task");
        assert_eq!(t.assignee.as_deref(), Some("at-dev-2"));
        assert!(
            t.routed_to.is_none(),
            "no routing for direct agent assignment"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn claim_clears_routed_to() {
        let home = tmp_home("claim_clears_rt");
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "fix", "assignee": "devs"}),
        );
        let id = list_all(&home)[0].id.clone();
        assert_eq!(list_all(&home)[0].routed_to.as_deref(), Some("lead"));
        handle(
            &home,
            "worker",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let t = &list_all(&home)[0];
        assert_eq!(t.assignee.as_deref(), Some("worker"));
        assert!(t.routed_to.is_none(), "claim should clear routed_to");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_assignee_re_resolves_routed_to() {
        let home = tmp_home("update_re_resolve");
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "alpha", "members": ["a1"], "orchestrator": "a1"}),
        );
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "beta", "members": ["b1"], "orchestrator": "b1"}),
        );
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "task", "assignee": "alpha"}),
        );
        let id = list_all(&home)[0].id.clone();
        assert_eq!(list_all(&home)[0].routed_to.as_deref(), Some("a1"));
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "update", "id": id, "assignee": "beta"}),
        );
        let t = &list_all(&home)[0];
        assert_eq!(t.assignee.as_deref(), Some("beta"));
        assert_eq!(t.routed_to.as_deref(), Some("b1"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_create_via_handle() {
        let home = tmp_home("board_create");
        let r = handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "new feature", "priority": "normal"}),
        );
        assert_eq!(r["status"], "created");
        let tasks = list_all(&home);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "new feature");
        assert_eq!(tasks[0].status, "open");
        assert_eq!(tasks[0].priority, "normal");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_move_status() {
        let home = tmp_home("board_move");
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "item", "priority": "low"}),
        );
        let tasks = list_all(&home);
        let id = &tasks[0].id;
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "update", "id": id, "priority": "normal"}),
        );
        let t = &list_all(&home)[0];
        assert_eq!(t.priority, "normal");
        assert_eq!(t.status, "open");
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "update", "id": id, "status": "claimed"}),
        );
        assert_eq!(list_all(&home)[0].status, "claimed");
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "update", "id": id, "status": "done"}),
        );
        assert_eq!(list_all(&home)[0].status, "done");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_assign_agent() {
        let home = tmp_home("board_assign");
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "fix bug"}),
        );
        let id = &list_all(&home)[0].id.clone();
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "update", "id": id, "assignee": "at-dev-2"}),
        );
        assert_eq!(list_all(&home)[0].assignee.as_deref(), Some("at-dev-2"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_cancel() {
        let home = tmp_home("board_cancel");
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "remove me"}),
        );
        let id = &list_all(&home)[0].id.clone();
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "update", "id": id, "status": "cancelled"}),
        );
        assert_eq!(list_all(&home)[0].status, "cancelled");
        let all = list_all(&home);
        let columns = crate::render::task_board_columns(&all);
        let total: usize = columns.iter().map(|c| c.len()).sum();
        assert_eq!(total, 0, "cancelled task should not appear in any column");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_shift_d_marks_done() {
        // Test Shift+D (done action) from all 3 non-done columns
        for (label, setup) in [
            (
                "backlog",
                vec![(
                    "create",
                    r#"{"action":"create","title":"t","priority":"low"}"#,
                )],
            ),
            (
                "open",
                vec![(
                    "create",
                    r#"{"action":"create","title":"t","priority":"normal"}"#,
                )],
            ),
            (
                "in_progress",
                vec![
                    (
                        "create",
                        r#"{"action":"create","title":"t","priority":"normal"}"#,
                    ),
                    ("claim", r#"{"action":"claim","id":"__ID__"}"#),
                ],
            ),
        ] {
            let home = tmp_home(&format!("shift_d_{label}"));
            let mut id = String::new();
            for (_, json_str) in &setup {
                let json_str = json_str.replace("__ID__", &id);
                let v: serde_json::Value =
                    serde_json::from_str(&json_str).expect("test JSON literal");
                let r = handle(&home, "user", &v);
                if let Some(i) = r["id"].as_str() {
                    id = i.to_string();
                }
            }
            if id.is_empty() {
                id = list_all(&home)[0].id.clone();
            }
            let r = handle(
                &home,
                "user",
                &serde_json::json!({"action": "done", "id": id}),
            );
            assert_eq!(r["status"], "done", "failed for {label}");
            assert_eq!(list_all(&home)[0].status, "done", "failed for {label}");
            std::fs::remove_dir_all(&home).ok();
        }
    }
}
