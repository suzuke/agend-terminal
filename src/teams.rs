//! Team management — named groups of instances for broadcast targeting.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub name: String,
    pub members: Vec<String>,
    /// Required orchestrator — must be ∈ members.
    #[serde(default)]
    pub orchestrator: Option<String>,
    pub description: Option<String>,
    pub created_at: String,
}

impl Team {
    pub fn is_degraded(&self) -> bool {
        self.orchestrator.is_none()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TeamStore {
    #[serde(default)]
    schema_version: u32,
    teams: Vec<Team>,
}

impl crate::store::SchemaVersioned for TeamStore {
    const CURRENT: u32 = 1;
    fn version_mut(&mut self) -> &mut u32 {
        &mut self.schema_version
    }
}

fn store_path(home: &Path) -> std::path::PathBuf {
    crate::store::store_path(home, "teams.json")
}

fn load(home: &Path) -> TeamStore {
    crate::store::load_versioned(
        &store_path(home),
        <TeamStore as crate::store::SchemaVersioned>::CURRENT,
    )
}

/// Find which team a member belongs to (one-agent-one-team).
fn find_team_for_member(store: &TeamStore, name: &str) -> Option<String> {
    store
        .teams
        .iter()
        .find(|t| t.members.contains(&name.to_string()))
        .map(|t| t.name.clone())
}

/// Return the full [`Team`] record for the team `member` belongs to,
/// or `None` when `member` isn't on any team. Used by
/// `api::handlers::prepare_instructions` to split agend.md's peer list
/// into team members vs other fleet agents.
pub fn find_team_for(home: &Path, member: &str) -> Option<Team> {
    let store = load(home);
    store
        .teams
        .into_iter()
        .find(|t| t.members.iter().any(|m| m == member))
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
    let orchestrator = args["orchestrator"].as_str().map(String::from);
    let description = args["description"].as_str().map(String::from);

    // Validate orchestrator
    if let Some(ref orch) = orchestrator {
        if !members.contains(orch) {
            return serde_json::json!({"error": format!("orchestrator '{orch}' must be a member")});
        }
    }

    let mut warnings: Vec<String> = Vec::new();

    match crate::store::mutate_versioned(&store_path(home), |store: &mut TeamStore| {
        if store.teams.iter().any(|t| t.name == name) {
            return Ok(false);
        }
        // One-agent-one-team check
        for m in &members {
            if let Some(existing_team) = find_team_for_member(store, m) {
                warnings.push(format!("member '{m}' already in team '{existing_team}'"));
                return Ok(false);
            }
        }
        store.teams.push(Team {
            name: name.clone(),
            members,
            orchestrator,
            description,
            created_at: chrono::Utc::now().to_rfc3339(),
        });
        Ok(true)
    }) {
        Ok(true) => {
            let mut result = serde_json::json!({"status": "created", "name": name});
            if !warnings.is_empty() {
                result["warnings"] = serde_json::json!(warnings);
            }
            result
        }
        Ok(false) => {
            if !warnings.is_empty() {
                serde_json::json!({"error": warnings[0]})
            } else {
                serde_json::json!({"error": format!("team '{name}' already exists")})
            }
        }
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

pub fn delete(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n.to_string(),
        None => return serde_json::json!({"error": "missing 'name'"}),
    };
    match crate::store::mutate_versioned(&store_path(home), |store: &mut TeamStore| {
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
    let teams: Vec<Value> = store
        .teams
        .iter()
        .map(|t| {
            let mut v = serde_json::to_value(t).unwrap_or_default();
            v["degraded"] = serde_json::json!(t.is_degraded());
            v
        })
        .collect();
    serde_json::json!({"teams": teams})
}

pub fn update(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n.to_string(),
        None => return serde_json::json!({"error": "missing 'name'"}),
    };
    let to_add: Vec<String> = args["add"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let to_remove: Vec<String> = args["remove"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let new_orchestrator = args["orchestrator"].as_str().map(String::from);

    // Pre-check: cannot remove orchestrator
    // (done inside mutate to see current state)

    match crate::store::mutate_versioned(&store_path(home), |store: &mut TeamStore| {
        let team_idx = match store.teams.iter().position(|t| t.name == name) {
            Some(i) => i,
            None => return Ok(false),
        };
        // Block removing the orchestrator
        if let Some(ref orch) = store.teams[team_idx].orchestrator {
            if to_remove.contains(orch) {
                return Ok(false);
            }
        }
        // One-agent-one-team check on adds
        for m in &to_add {
            if let Some(existing) = find_team_for_member(store, m) {
                if existing != name {
                    return Ok(false);
                }
            }
        }
        let team = &mut store.teams[team_idx];
        for m in &to_add {
            if !team.members.contains(m) {
                team.members.push(m.clone());
            }
        }
        team.members.retain(|m| !to_remove.contains(m));
        // Change orchestrator
        if let Some(ref new_orch) = new_orchestrator {
            if !team.members.contains(new_orch) {
                return Ok(false);
            }
            team.orchestrator = Some(new_orch.clone());
        }
        Ok(true)
    }) {
        Ok(true) => serde_json::json!({"status": "updated", "name": name}),
        Ok(false) => {
            // Determine specific error
            let store = load(home);
            let team = match store.teams.iter().find(|t| t.name == name) {
                Some(t) => t,
                None => return serde_json::json!({"error": format!("team '{name}' not found")}),
            };
            if let Some(ref orch) = team.orchestrator {
                if to_remove.contains(orch) {
                    return serde_json::json!({"error": format!("cannot remove orchestrator '{orch}'; use update_team --orchestrator to reassign first")});
                }
            }
            for m in &to_add {
                if let Some(existing) = find_team_for_member(&store, m) {
                    if existing != name {
                        return serde_json::json!({"error": format!("member '{m}' already in team '{existing}'")});
                    }
                }
            }
            if let Some(ref new_orch) = new_orchestrator {
                if !team.members.contains(new_orch) {
                    return serde_json::json!({"error": format!("new orchestrator '{new_orch}' must be a current member")});
                }
            }
            serde_json::json!({"error": "update failed"})
        }
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

/// Remove an instance from ALL teams. Auto-delete teams that become empty.
pub fn remove_member_from_all(home: &Path, instance_name: &str) {
    let mut degraded_teams: Vec<String> = Vec::new();
    let _ = crate::store::mutate_versioned(&store_path(home), |store: &mut TeamStore| {
        for team in &mut store.teams {
            team.members.retain(|m| m != instance_name);
            if team.orchestrator.as_deref() == Some(instance_name) {
                team.orchestrator = None;
                degraded_teams.push(team.name.clone());
            }
        }
        store.teams.retain(|t| !t.members.is_empty());
        Ok(true)
    });
    // Create urgent task for each newly degraded team
    for team_name in &degraded_teams {
        crate::tasks::handle(
            home,
            "system",
            &serde_json::json!({
                "action": "create",
                "title": format!("Team '{team_name}' needs new orchestrator ('{instance_name}' was deleted)"),
                "priority": "urgent",
            }),
        );
    }
}

/// Reconcile teams from fleet.yaml seed config. Additive only — runtime-added
/// members are preserved; only missing seed members are added.
pub fn reconcile_teams(home: &Path, fleet: &crate::fleet::FleetConfig) {
    for (name, seed) in &fleet.teams {
        let existing = get_members(home, name);
        if existing.is_empty() {
            create(
                home,
                &serde_json::json!({
                    "name": name,
                    "members": seed.members,
                    "orchestrator": seed.orchestrator,
                    "description": seed.description,
                }),
            );
        } else {
            let missing: Vec<&String> = seed
                .members
                .iter()
                .filter(|m| !existing.contains(m))
                .collect();
            if !missing.is_empty() {
                update(home, &serde_json::json!({ "name": name, "add": missing }));
            }
            // Reconcile orchestrator if set in fleet.yaml but not in store
            if let Some(ref orch) = seed.orchestrator {
                let store = load(home);
                if let Some(team) = store.teams.iter().find(|t| t.name == *name) {
                    if team.orchestrator.is_none() {
                        update(
                            home,
                            &serde_json::json!({ "name": name, "orchestrator": orch }),
                        );
                    }
                }
            }
        }
    }
}

/// Resolve an assignee name: if it's a team, return the orchestrator.
/// Returns Ok(Some(orchestrator)) if team found, Ok(None) if not a team,
/// Err if team is degraded.
pub fn resolve_team_orchestrator(home: &Path, name: &str) -> Result<Option<String>, String> {
    let store = load(home);
    match store.teams.iter().find(|t| t.name == name) {
        Some(team) => match &team.orchestrator {
            Some(orch) => Ok(Some(orch.clone())),
            None => Err(format!(
                "team '{name}' is degraded (no orchestrator), cannot route task"
            )),
        },
        None => Ok(None), // not a team name
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
            &serde_json::json!({"name": "devs", "members": ["a", "b"], "orchestrator": "a"}),
        );
        assert_eq!(r["status"], "created");

        let listed = list(&home);
        assert_eq!(listed["teams"].as_array().expect("arr").len(), 1);
        assert_eq!(
            listed["teams"][0]["members"].as_array().expect("m").len(),
            2
        );
        assert_eq!(listed["teams"][0]["orchestrator"], "a");

        // Add member
        update(&home, &serde_json::json!({"name": "devs", "add": ["c"]}));
        let members = get_members(&home, "devs");
        assert_eq!(members, vec!["a", "b", "c"]);

        // Remove non-orchestrator member
        update(&home, &serde_json::json!({"name": "devs", "remove": ["b"]}));
        let members = get_members(&home, "devs");
        assert_eq!(members, vec!["a", "c"]);

        // Duplicate add ignored
        update(&home, &serde_json::json!({"name": "devs", "add": ["a"]}));
        let members = get_members(&home, "devs");
        assert_eq!(members, vec!["a", "c"]);

        // Delete
        let r = delete(&home, &serde_json::json!({"name": "devs"}));
        assert_eq!(r["status"], "deleted");
        assert!(list(&home)["teams"].as_array().expect("arr").is_empty());

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_duplicate_create() {
        let home = tmp_home("dup_create");
        create(
            &home,
            &serde_json::json!({"name": "t", "members": ["a"], "orchestrator": "a"}),
        );
        let r = create(
            &home,
            &serde_json::json!({"name": "t", "members": ["b"], "orchestrator": "b"}),
        );
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

    #[test]
    fn create_team_requires_orchestrator_in_members() {
        let home = tmp_home("orch_not_member");
        let r = create(
            &home,
            &serde_json::json!({"name": "t", "members": ["a", "b"], "orchestrator": "c"}),
        );
        assert!(
            r["error"]
                .as_str()
                .expect("err")
                .contains("must be a member"),
            "got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn one_agent_one_team_rejects_duplicate() {
        let home = tmp_home("one_agent_one_team");
        create(
            &home,
            &serde_json::json!({"name": "alpha", "members": ["alice"], "orchestrator": "alice"}),
        );
        let r = create(
            &home,
            &serde_json::json!({"name": "beta", "members": ["alice", "bob"], "orchestrator": "bob"}),
        );
        assert!(
            r["error"]
                .as_str()
                .expect("err")
                .contains("already in team"),
            "got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_team_change_orchestrator() {
        let home = tmp_home("change_orch");
        create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["a", "b"], "orchestrator": "a"}),
        );
        let r = update(
            &home,
            &serde_json::json!({"name": "devs", "orchestrator": "b"}),
        );
        assert_eq!(r["status"], "updated");
        let listed = list(&home);
        assert_eq!(listed["teams"][0]["orchestrator"], "b");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_team_cannot_remove_orchestrator() {
        let home = tmp_home("remove_orch");
        create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["a", "b"], "orchestrator": "a"}),
        );
        let r = update(&home, &serde_json::json!({"name": "devs", "remove": ["a"]}));
        assert!(
            r["error"]
                .as_str()
                .expect("err")
                .contains("cannot remove orchestrator"),
            "got: {r}"
        );
        // Verify member still there
        assert!(get_members(&home, "devs").contains(&"a".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

    fn make_fleet(teams: &[(&str, &[&str], Option<&str>)]) -> crate::fleet::FleetConfig {
        let mut map = std::collections::HashMap::new();
        for (name, members, orch) in teams {
            map.insert(
                name.to_string(),
                crate::fleet::TeamConfig {
                    members: members.iter().map(|s| s.to_string()).collect(),
                    orchestrator: orch.map(|s| s.to_string()),
                    description: None,
                },
            );
        }
        crate::fleet::FleetConfig {
            teams: map,
            ..Default::default()
        }
    }

    #[test]
    fn fleet_yaml_teams_creates_on_startup() {
        let home = tmp_home("reconcile_create");
        let fleet = make_fleet(&[("devs", &["alice", "bob"], Some("alice"))]);
        reconcile_teams(&home, &fleet);
        let members = get_members(&home, "devs");
        assert_eq!(members, vec!["alice", "bob"]);
        let listed = list(&home);
        assert_eq!(listed["teams"][0]["orchestrator"], "alice");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_yaml_teams_additive_reconcile() {
        let home = tmp_home("reconcile_additive");
        create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["alice", "runtime-extra"], "orchestrator": "alice"}),
        );
        let fleet = make_fleet(&[("devs", &["alice", "bob"], Some("alice"))]);
        reconcile_teams(&home, &fleet);
        let members = get_members(&home, "devs");
        assert!(members.contains(&"alice".to_string()));
        assert!(members.contains(&"bob".to_string()));
        assert!(members.contains(&"runtime-extra".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_yaml_teams_idempotent() {
        let home = tmp_home("reconcile_idempotent");
        let fleet = make_fleet(&[("devs", &["alice"], Some("alice"))]);
        reconcile_teams(&home, &fleet);
        reconcile_teams(&home, &fleet);
        let listed = list(&home);
        let teams = listed["teams"].as_array().expect("teams");
        assert_eq!(teams.len(), 1, "should not duplicate team");
        assert_eq!(get_members(&home, "devs"), vec!["alice"]);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_yaml_reconcile_with_orchestrator() {
        let home = tmp_home("reconcile_orch");
        let fleet = make_fleet(&[("ops", &["lead", "worker"], Some("lead"))]);
        reconcile_teams(&home, &fleet);
        let listed = list(&home);
        assert_eq!(listed["teams"][0]["orchestrator"], "lead");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn create_without_orchestrator_still_works() {
        // Backward compat: orchestrator is optional at data level
        let home = tmp_home("no_orch");
        let r = create(&home, &serde_json::json!({"name": "t", "members": ["a"]}));
        assert_eq!(r["status"], "created");
        let listed = list(&home);
        assert!(listed["teams"][0]["orchestrator"].is_null());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn delete_orchestrator_clears_team_orchestrator() {
        let home = tmp_home("del_orch_clears");
        create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        remove_member_from_all(&home, "lead");
        let listed = list(&home);
        let teams = listed["teams"].as_array().expect("teams");
        assert_eq!(teams.len(), 1, "team should survive (worker remains)");
        assert!(
            teams[0]["orchestrator"].is_null(),
            "orchestrator must be cleared when removed: got {:?}",
            teams[0]["orchestrator"]
        );
        assert_eq!(teams[0]["members"].as_array().expect("m").len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn delete_orchestrator_creates_urgent_task() {
        let home = tmp_home("del_orch_task");
        create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        remove_member_from_all(&home, "lead");
        let tasks = crate::tasks::list_all(&home);
        let urgent: Vec<_> = tasks
            .iter()
            .filter(|t| t.priority == "urgent" && t.title.contains("needs new orchestrator"))
            .collect();
        assert_eq!(urgent.len(), 1, "should create exactly one urgent task");
        assert!(
            urgent[0].title.contains("devs"),
            "task should mention team name"
        );
        assert!(
            urgent[0].title.contains("lead"),
            "task should mention removed orchestrator"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn degraded_team_shows_in_list() {
        let home = tmp_home("degraded_list");
        create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        // Before removal: not degraded
        let listed = list(&home);
        assert_eq!(listed["teams"][0]["degraded"], false);

        remove_member_from_all(&home, "lead");
        let listed = list(&home);
        assert_eq!(listed["teams"][0]["degraded"], true);
        std::fs::remove_dir_all(&home).ok();
    }
}
