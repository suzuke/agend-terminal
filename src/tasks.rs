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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
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

/// Check if an instance name is known (in fleet.yaml).
/// Returns true if fleet.yaml doesn't exist (no fleet = no restriction).
fn instance_exists(home: &Path, name: &str) -> bool {
    let fleet_path = home.join("fleet.yaml");
    if !fleet_path.exists() {
        return true; // no fleet config = no restriction
    }
    crate::fleet::FleetConfig::load(&fleet_path)
        .map(|c| c.instances.contains_key(name))
        .unwrap_or(true) // parse error = permissive
}

/// Check if caller is allowed to mutate a task (assignee or orchestrator).
/// Unassigned tasks can be mutated by anyone.
fn can_mutate_task(home: &Path, caller: &str, task: &Task) -> bool {
    match &task.assignee {
        None => true,
        Some(assignee) => {
            if assignee == caller {
                return true;
            }
            // Check if caller is orchestrator of assignee's team
            if crate::teams::is_orchestrator_of(home, caller, assignee) {
                return true;
            }
            // Check if assignee is a team name and caller is its orchestrator
            if let Ok(Some(orch)) = crate::teams::resolve_team_orchestrator(home, assignee) {
                if orch == caller {
                    return true;
                }
            }
            false
        }
    }
}

fn load(home: &Path) -> TaskStore {
    crate::store::load_versioned(
        &store_path(home),
        <TaskStore as crate::store::SchemaVersioned>::CURRENT,
    )
}

/// Evaluate dependency status for a single task.
/// Returns the effective status after considering depends_on:
/// - open + any dep not done → "blocked"
/// - blocked + all deps done → "open" (auto-unblock)
/// - claimed/done/cancelled → unchanged
///
/// Uses a visited set to prevent infinite loops on circular deps
/// (circular → treated as blocked).
pub fn evaluate_dependency_status(tasks: &[Task], task: &Task) -> String {
    if task.depends_on.is_empty()
        || matches!(task.status.as_str(), "claimed" | "done" | "cancelled")
    {
        return task.status.clone();
    }
    let all_deps_done = task.depends_on.iter().all(|dep_id| {
        tasks
            .iter()
            .find(|t| t.id == *dep_id)
            .map(|t| t.status == "done")
            .unwrap_or(false) // missing dep → not done → blocked
    });
    if all_deps_done {
        if task.status == "blocked" {
            "open".to_string()
        } else {
            task.status.clone()
        }
    } else {
        "blocked".to_string()
    }
}

/// Apply dependency evaluation to all tasks in a store, mutating statuses.
/// Returns true if any status changed.
fn apply_dependency_eval(tasks: &mut [Task]) -> bool {
    let snapshot: Vec<Task> = tasks.to_vec();
    let mut changed = false;
    for task in tasks.iter_mut() {
        let effective = evaluate_dependency_status(&snapshot, task);
        if effective != task.status {
            task.status = effective;
            task.updated_at = chrono::Utc::now().to_rfc3339();
            changed = true;
        }
    }
    changed
}

/// Return all tasks as typed structs (no JSON round-trip).
pub fn list_all(home: &Path) -> Vec<Task> {
    load(home).tasks
}

/// Sweep overdue claimed tasks back to open.
/// Returns the IDs of tasks that were unclaimed.
pub fn sweep_overdue_claimed(home: &Path) -> Vec<String> {
    let now = chrono::Utc::now();
    let mut unclaimed = Vec::new();
    let _ = crate::store::mutate_versioned(&store_path(home), |store: &mut TaskStore| {
        for task in store.tasks.iter_mut() {
            if task.status != "claimed" {
                continue;
            }
            let due = match &task.due_at {
                Some(d) => d,
                None => continue,
            };
            let due_utc = match chrono::DateTime::parse_from_rfc3339(due) {
                Ok(dt) => dt.with_timezone(&chrono::Utc),
                Err(_) => continue,
            };
            if now > due_utc {
                task.status = "open".to_string();
                task.assignee = None;
                task.routed_to = None;
                task.updated_at = now.to_rfc3339();
                unclaimed.push(task.id.clone());
            }
        }
        Ok(())
    });
    unclaimed
}

fn parse_due_at(args: &Value) -> Option<String> {
    if let Some(due) = args["due_at"].as_str() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(due) {
            return Some(dt.with_timezone(&chrono::Utc).to_rfc3339());
        }
    }
    if let Some(dur) = args["duration"].as_str() {
        if let Some(d) = parse_duration(dur) {
            return Some((chrono::Utc::now() + d).to_rfc3339());
        }
    }
    None
}

fn parse_duration(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num.parse().ok()?;
    match unit {
        "m" => Some(chrono::Duration::minutes(n)),
        "h" => Some(chrono::Duration::hours(n)),
        "d" => Some(chrono::Duration::days(n)),
        _ => None,
    }
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
            use std::sync::atomic::{AtomicU64, Ordering};
            static ID_SEQ: AtomicU64 = AtomicU64::new(0);
            let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
            let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
            let id = format!("t-{ts}-{seq}");
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
                due_at: parse_due_at(args),
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
            // Re-evaluate dependency status and persist changes
            let _ = crate::store::mutate_versioned(&store_path(home), |store: &mut TaskStore| {
                apply_dependency_eval(&mut store.tasks);
                Ok(())
            });
            let store = load(home);
            let filter_assignee = args["filter_assignee"].as_str();
            let filter_status = args["filter_status"].as_str();
            let now = chrono::Utc::now();
            let done_ttl = chrono::Duration::days(14);
            let filtered: Vec<_> = store
                .tasks
                .iter()
                .filter(|t| filter_assignee.is_none_or(|a| t.assignee.as_deref() == Some(a)))
                .filter(|t| filter_status.is_none_or(|s| t.status == s))
                .filter(|t| {
                    // When no explicit status filter, hide done tasks older than 14d
                    if filter_status.is_some() || t.status != "done" {
                        return true;
                    }
                    chrono::DateTime::parse_from_rfc3339(&t.updated_at)
                        .map(|dt| {
                            now.signed_duration_since(dt.with_timezone(&chrono::Utc)) < done_ttl
                        })
                        .unwrap_or(true)
                })
                .collect();
            serde_json::json!({"tasks": filtered})
        }
        "claim" => {
            let id = match args["id"].as_str() {
                Some(i) => i.to_string(),
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            let iname = instance_name.to_string();
            // Verify caller is a known instance
            if !instance_exists(home, &iname) {
                return serde_json::json!({"error": format!("instance '{iname}' not found in fleet.yaml")});
            }
            match crate::store::mutate_versioned(&store_path(home), |store: &mut TaskStore| {
                match store.tasks.iter_mut().find(|t| t.id == id) {
                    Some(task) => {
                        // Self re-claim is always ok
                        if task.status == "claimed" && task.assignee.as_deref() == Some(&iname) {
                            // no-op re-claim, just update timestamp
                        } else if task.status != "open" {
                            anyhow::bail!(
                                "task '{id}' status is '{}', only 'open' tasks can be claimed",
                                task.status
                            );
                        }
                        task.status = "claimed".to_string();
                        task.assignee = Some(iname.clone());
                        task.routed_to = None;
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
            let caller = instance_name.to_string();
            match crate::store::mutate_versioned(&store_path(home), |store: &mut TaskStore| {
                match store.tasks.iter_mut().find(|t| t.id == id) {
                    Some(task) => {
                        if !can_mutate_task(home, &caller, task) {
                            anyhow::bail!(
                                "task '{id}' owned by '{}', caller '{caller}' not authorized",
                                task.assignee.as_deref().unwrap_or("unassigned")
                            );
                        }
                        task.status = "done".to_string();
                        task.result.clone_from(&result_text);
                        task.updated_at = chrono::Utc::now().to_rfc3339();
                        apply_dependency_eval(&mut store.tasks);
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
            let caller = instance_name.to_string();
            match crate::store::mutate_versioned(&store_path(home), |store: &mut TaskStore| {
                match store.tasks.iter_mut().find(|t| t.id == id) {
                    Some(task) => {
                        if !can_mutate_task(home, &caller, task) {
                            anyhow::bail!(
                                "task '{id}' owned by '{}', caller '{caller}' not authorized",
                                task.assignee.as_deref().unwrap_or("unassigned")
                            );
                        }
                        if let Some(ref s) = new_status {
                            task.status = s.clone();
                            // Release claim: setting status=open clears ownership
                            if s == "open" {
                                task.assignee = None;
                                task.routed_to = None;
                            }
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
            "a1",
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

    #[test]
    fn test_concurrent_creates_unique_ids() {
        let home = tmp_home("concurrent_ids");
        let home_arc = std::sync::Arc::new(home.clone());
        let threads: Vec<_> = (0..20)
            .map(|i| {
                let h = home_arc.clone();
                std::thread::spawn(move || {
                    handle(
                        &h,
                        &format!("agent-{i}"),
                        &serde_json::json!({"action": "create", "title": format!("task-{i}")}),
                    )
                })
            })
            .collect();
        let ids: Vec<String> = threads
            .into_iter()
            .map(|h| {
                let r = h.join().expect("thread");
                assert_eq!(r["status"], "created");
                r["id"].as_str().expect("id").to_string()
            })
            .collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            20,
            "all 20 task IDs must be unique, got: {ids:?}"
        );
        let tasks = list_all(&home);
        assert_eq!(tasks.len(), 20);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_blocked_when_dep_not_done() {
        let home = tmp_home("dep-blocked");
        // Create dep task (stays open)
        let r1 = handle(
            &home,
            "u",
            &serde_json::json!({"action": "create", "title": "dep"}),
        );
        let dep_id = r1["id"].as_str().unwrap().to_string();
        // Create task depending on dep
        handle(
            &home,
            "u",
            &serde_json::json!({
                "action": "create", "title": "child", "depends_on": [dep_id]
            }),
        );
        // List triggers eval → child should be blocked
        let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
        let tasks = listed["tasks"].as_array().unwrap();
        let child = tasks.iter().find(|t| t["title"] == "child").unwrap();
        assert_eq!(
            child["status"], "blocked",
            "task with open dep must be blocked"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_auto_unblock_when_all_deps_done() {
        let home = tmp_home("dep-unblock");
        let r1 = handle(
            &home,
            "u",
            &serde_json::json!({"action": "create", "title": "dep"}),
        );
        let dep_id = r1["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "u",
            &serde_json::json!({
                "action": "create", "title": "child", "depends_on": [dep_id]
            }),
        );
        // List → child blocked
        let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
        let child = listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["title"] == "child")
            .unwrap();
        assert_eq!(child["status"], "blocked");

        // Complete dep → done triggers re-eval → child auto-unblocks
        handle(
            &home,
            "u",
            &serde_json::json!({"action": "done", "id": dep_id, "result": "ok"}),
        );
        let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
        let child = listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["title"] == "child")
            .unwrap();
        assert_eq!(
            child["status"], "open",
            "child must auto-unblock when dep is done"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_claimed_task_not_touched_by_dep_eval() {
        let home = tmp_home("dep-claimed");
        let r1 = handle(
            &home,
            "u",
            &serde_json::json!({"action": "create", "title": "dep"}),
        );
        let dep_id = r1["id"].as_str().unwrap().to_string();
        let r2 = handle(
            &home,
            "u",
            &serde_json::json!({
                "action": "create", "title": "child", "depends_on": [dep_id]
            }),
        );
        let child_id = r2["id"].as_str().unwrap().to_string();
        // Claim the child (impl started working despite dep)
        handle(
            &home,
            "impl",
            &serde_json::json!({"action": "claim", "id": child_id}),
        );
        // List → claimed task must stay claimed, not flipped to blocked
        let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
        let child = listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["title"] == "child")
            .unwrap();
        assert_eq!(
            child["status"], "claimed",
            "claimed task must not be touched by dep eval"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_circular_dep_no_infinite_loop() {
        let home = tmp_home("dep-circular");
        // Create two tasks that depend on each other
        let r1 = handle(
            &home,
            "u",
            &serde_json::json!({"action": "create", "title": "A"}),
        );
        let id_a = r1["id"].as_str().unwrap().to_string();
        let r2 = handle(
            &home,
            "u",
            &serde_json::json!({"action": "create", "title": "B", "depends_on": [id_a]}),
        );
        let id_b = r2["id"].as_str().unwrap().to_string();
        // Update A to depend on B (circular)
        handle(
            &home,
            "u",
            &serde_json::json!({"action": "update", "id": id_a, "status": "open"}),
        );
        // Manually set depends_on for A → B via mutate
        let _ = crate::store::mutate_versioned(&store_path(&home), |store: &mut TaskStore| {
            if let Some(t) = store.tasks.iter_mut().find(|t| t.id == id_a) {
                t.depends_on = vec![id_b.clone()];
            }
            Ok(())
        });
        // List must not hang — both should be blocked (neither is done)
        let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
        let tasks = listed["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2, "must return without infinite loop");
        for t in tasks {
            assert_eq!(
                t["status"], "blocked",
                "circular dep tasks must be blocked: {}",
                t["title"]
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_create_accepts_due_at_iso() {
        let home = tmp_home("due-at-iso");
        let future = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let result = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "timed", "due_at": future}),
        );
        assert_eq!(result["status"], "created");
        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        let task = &listed["tasks"][0];
        assert!(task["due_at"].is_string(), "due_at must be set");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_overdue_claimed_task_unclaimed_by_sweep() {
        let home = tmp_home("overdue-sweep");
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let r = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "overdue", "due_at": past}),
        );
        let id = r["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let unclaimed = sweep_overdue_claimed(&home);
        assert_eq!(unclaimed, vec![id.clone()]);
        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        let task = &listed["tasks"][0];
        assert_eq!(task["status"], "open");
        assert!(task["assignee"].is_null());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_not_yet_due_not_touched() {
        let home = tmp_home("not-due");
        let future = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let r = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "future", "due_at": future}),
        );
        let id = r["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let unclaimed = sweep_overdue_claimed(&home);
        assert!(unclaimed.is_empty(), "future task must not be unclaimed");
        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        assert_eq!(listed["tasks"][0]["status"], "claimed");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_done_task_ignored_by_sweep() {
        let home = tmp_home("done-ignore");
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let r = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "done-overdue", "due_at": past}),
        );
        let id = r["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "done", "id": id, "result": "finished"}),
        );
        let unclaimed = sweep_overdue_claimed(&home);
        assert!(unclaimed.is_empty(), "done task must not be unclaimed");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_create_accepts_duration_30m() {
        let home = tmp_home("dur-30m");
        let before = chrono::Utc::now();
        let result = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "timed", "duration": "30m"}),
        );
        assert_eq!(result["status"], "created");
        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        let due_str = listed["tasks"][0]["due_at"].as_str().expect("due_at set");
        let due = chrono::DateTime::parse_from_rfc3339(due_str)
            .expect("valid rfc3339")
            .with_timezone(&chrono::Utc);
        let expected = before + chrono::Duration::minutes(30);
        let diff = (due - expected).num_seconds().abs();
        assert!(diff < 5, "due_at should be ~now+30m, diff={diff}s");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_create_duration_variants() {
        let home = tmp_home("dur-variants");
        let now = chrono::Utc::now();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "1h", "duration": "1h"}),
        );
        let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
        let due =
            chrono::DateTime::parse_from_rfc3339(listed["tasks"][0]["due_at"].as_str().unwrap())
                .unwrap()
                .with_timezone(&chrono::Utc);
        assert!((due - now).num_minutes() >= 59);
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "2d", "duration": "2d"}),
        );
        let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
        let due =
            chrono::DateTime::parse_from_rfc3339(listed["tasks"][1]["due_at"].as_str().unwrap())
                .unwrap()
                .with_timezone(&chrono::Utc);
        assert!((due - now).num_hours() >= 47);
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "bad", "duration": "xyz"}),
        );
        let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
        assert!(
            listed["tasks"][2]["due_at"].is_null(),
            "invalid duration → no due_at"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_daemon_maintenance_unclaims_overdue_and_logs_event() {
        let home = tmp_home("daemon-maint");
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let r = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "overdue-maint", "due_at": past}),
        );
        let id = r["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        crate::daemon::run_task_maintenance(&home);
        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        assert_eq!(listed["tasks"][0]["status"], "open");
        assert!(listed["tasks"][0]["assignee"].is_null());
        let log_content = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log_content.contains("task_overdue_unclaimed"),
            "event_log must contain task_overdue_unclaimed entry"
        );
        assert!(
            log_content.contains(&id),
            "event_log must reference the task id"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Mutation integrity tests ──

    fn write_fleet_yaml(home: &std::path::Path, instances: &[&str]) {
        let entries: Vec<String> = instances
            .iter()
            .map(|n| format!("  {n}:\n    backend: claude"))
            .collect();
        let yaml = format!("instances:\n{}", entries.join("\n"));
        std::fs::write(home.join("fleet.yaml"), yaml).ok();
    }

    #[test]
    fn test_claim_unknown_instance_rejected() {
        let home = tmp_home("claim-unknown");
        write_fleet_yaml(&home, &["known-agent"]);
        let r = handle(
            &home,
            "known-agent",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        // Unknown instance tries to claim
        let r = handle(
            &home,
            "phantom",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("not found in fleet"),
            "got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_claim_already_claimed_by_other_rejected() {
        let home = tmp_home("claim-stolen");
        write_fleet_yaml(&home, &["agent-a", "agent-b"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // agent-b tries to steal
        let r = handle(
            &home,
            "agent-b",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("only 'open'"),
            "claimed task must not be claimable by others: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_claim_self_reclaim_ok() {
        let home = tmp_home("claim-reclaim");
        write_fleet_yaml(&home, &["agent-a"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // Re-claim own task → ok
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        assert_eq!(r["status"], "claimed", "self re-claim must succeed");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_done_non_assignee_rejected() {
        let home = tmp_home("done-non-assignee");
        write_fleet_yaml(&home, &["agent-a", "agent-b"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // agent-b tries to mark done
        let r = handle(
            &home,
            "agent-b",
            &serde_json::json!({"action": "done", "id": id}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("not authorized"),
            "got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_done_assignee_ok() {
        let home = tmp_home("done-assignee");
        write_fleet_yaml(&home, &["agent-a"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "done", "id": id, "result": "ok"}),
        );
        assert_eq!(r["status"], "done");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_done_orchestrator_ok() {
        let home = tmp_home("done-orch");
        write_fleet_yaml(&home, &["lead", "worker"]);
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "dev", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        let r = handle(
            &home,
            "lead",
            &serde_json::json!({"action": "create", "title": "t", "assignee": "worker"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "worker",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // Orchestrator marks done on behalf
        let r = handle(
            &home,
            "lead",
            &serde_json::json!({"action": "done", "id": id, "result": "merged"}),
        );
        assert_eq!(
            r["status"], "done",
            "orchestrator must be able to mark done"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_update_non_owner_rejected() {
        let home = tmp_home("update-non-owner");
        write_fleet_yaml(&home, &["agent-a", "agent-b"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // agent-b tries to change priority
        let r = handle(
            &home,
            "agent-b",
            &serde_json::json!({"action": "update", "id": id, "priority": "urgent"}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("not authorized"),
            "got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_update_orchestrator_ok() {
        let home = tmp_home("update-orch");
        write_fleet_yaml(&home, &["lead", "worker"]);
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "dev", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        let r = handle(
            &home,
            "lead",
            &serde_json::json!({"action": "create", "title": "t", "assignee": "worker"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "worker",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let r = handle(
            &home,
            "lead",
            &serde_json::json!({"action": "update", "id": id, "priority": "urgent"}),
        );
        assert_eq!(
            r["status"], "updated",
            "orchestrator must be able to update"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_update_release_claim_ok() {
        let home = tmp_home("update-release");
        write_fleet_yaml(&home, &["agent-a"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // Release claim by setting status=open
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "update", "id": id, "status": "open"}),
        );
        assert_eq!(r["status"], "updated");
        let tasks = list_all(&home);
        let t = tasks.iter().find(|t| t.id == id).unwrap();
        assert_eq!(t.status, "open");
        assert!(t.assignee.is_none(), "release must clear assignee");
        assert!(t.routed_to.is_none(), "release must clear routed_to");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_claim_blocked_task_rejected() {
        let home = tmp_home("claim-blocked");
        write_fleet_yaml(&home, &["agent-a"]);
        // Create dep (stays open) + child that depends on it (auto-blocked)
        let r1 = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "dep"}),
        );
        let dep_id = r1["id"].as_str().unwrap().to_string();
        let r2 = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "child", "depends_on": [dep_id]}),
        );
        let child_id = r2["id"].as_str().unwrap();
        // List triggers dep eval → child becomes blocked
        handle(&home, "agent-a", &serde_json::json!({"action": "list"}));
        // Try to claim blocked task
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": child_id}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("only 'open'"),
            "blocked task must not be claimable: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_update_non_owner_on_open_assigned_rejected() {
        let home = tmp_home("update-non-owner-assigned");
        write_fleet_yaml(&home, &["agent-a", "agent-b"]);
        // Create task assigned to agent-a (but not claimed yet → status open)
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t", "assignee": "agent-a"}),
        );
        let id = r["id"].as_str().unwrap();
        // agent-b tries to change priority on agent-a's assigned task
        let r = handle(
            &home,
            "agent-b",
            &serde_json::json!({"action": "update", "id": id, "priority": "urgent"}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("not authorized"),
            "non-owner must not update assigned task: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // --- Sprint 8 PR-M: Done TTL filter ---

    #[test]
    fn test_list_default_hides_done_older_than_14d() {
        let home = tmp_home("done-ttl-hide");
        // Create two tasks, mark both done
        let r1 = handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "old done"}),
        );
        let id1 = r1["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "claim", "id": id1}),
        );
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "done", "id": id1, "result": "ok"}),
        );

        let r2 = handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "recent done"}),
        );
        let id2 = r2["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "claim", "id": id2}),
        );
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "done", "id": id2, "result": "ok"}),
        );

        // Backdate the first task's updated_at to 15 days ago
        let _ = crate::store::mutate_versioned(
            &crate::store::store_path(&home, "tasks.json"),
            |store: &mut TaskStore| {
                if let Some(t) = store.tasks.iter_mut().find(|t| t.id == id1) {
                    t.updated_at = (chrono::Utc::now() - chrono::Duration::days(15)).to_rfc3339();
                }
                Ok(())
            },
        );

        // Default list (no filter_status) should hide the old done task
        let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
        let tasks = listed["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1, "old done task must be hidden");
        assert_eq!(tasks[0]["title"], "recent done");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_list_done_filter_returns_all() {
        let home = tmp_home("done-ttl-all");
        let r1 = handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "old"}),
        );
        let id1 = r1["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "claim", "id": id1}),
        );
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "done", "id": id1, "result": "ok"}),
        );

        let r2 = handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "new"}),
        );
        let id2 = r2["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "claim", "id": id2}),
        );
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "done", "id": id2, "result": "ok"}),
        );

        let _ = crate::store::mutate_versioned(
            &crate::store::store_path(&home, "tasks.json"),
            |store: &mut TaskStore| {
                if let Some(t) = store.tasks.iter_mut().find(|t| t.id == id1) {
                    t.updated_at = (chrono::Utc::now() - chrono::Duration::days(15)).to_rfc3339();
                }
                Ok(())
            },
        );

        // Explicit filter_status=done returns ALL done tasks regardless of age
        let listed = handle(
            &home,
            "a",
            &serde_json::json!({"action": "list", "filter_status": "done"}),
        );
        let tasks = listed["tasks"].as_array().unwrap();
        assert_eq!(
            tasks.len(),
            2,
            "filter_status=done must return all done tasks"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_list_non_done_always_returns() {
        let home = tmp_home("done-ttl-nondone");
        // Create open + claimed tasks — they should always appear
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "open task"}),
        );
        let r2 = handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "claimed task"}),
        );
        let id2 = r2["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "claim", "id": id2}),
        );

        let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
        let tasks = listed["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2, "non-done tasks must always appear");
        std::fs::remove_dir_all(&home).ok();
    }
}
