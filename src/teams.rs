//! Team management — named groups of instances for broadcast targeting.
//!
//! Sprint 54 fleet-yaml unification: fleet.yaml `teams:` section is now
//! the canonical store. Runtime CRUD writes there directly via
//! `crate::fleet` helpers; the legacy `teams.json` runtime store is
//! migrated one-shot at daemon startup
//! (`crate::fleet::migrate_teams_json_to_yaml`) and renamed to
//! `teams.json.migrated`. `reconcile_teams` (the previous
//! seed-into-runtime bridge) is gone — operator-edited fleet.yaml is
//! the source of truth, no separate normalization phase.

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

fn load_fleet(home: &Path) -> crate::fleet::FleetConfig {
    crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).unwrap_or_default()
}

/// Project a fleet.yaml `(name, TeamConfig)` pair into the public `Team`
/// JSON shape used by `list` / `find_team_for`. `created_at` defaults
/// to empty string when absent (operator-edited fleet.yaml entries may
/// omit it; runtime-created teams always stamp it).
fn project_team(name: &str, cfg: &crate::fleet::TeamConfig) -> Team {
    Team {
        name: name.to_string(),
        members: cfg.members.clone(),
        orchestrator: cfg.orchestrator.clone(),
        description: cfg.description.clone(),
        created_at: cfg.created_at.clone().unwrap_or_default(),
    }
}

/// Find which team a member belongs to (one-agent-one-team invariant).
fn find_team_for_member(fleet: &crate::fleet::FleetConfig, name: &str) -> Option<String> {
    fleet
        .teams
        .iter()
        .find(|(_, cfg)| cfg.members.iter().any(|m| m == name))
        .map(|(team_name, _)| team_name.clone())
}

/// Return the full [`Team`] record for the team `member` belongs to,
/// or `None` when `member` isn't on any team. Used by
/// `api::handlers::prepare_instructions` to split agend.md's peer list
/// into team members vs other fleet agents.
pub fn find_team_for(home: &Path, member: &str) -> Option<Team> {
    let fleet = load_fleet(home);
    fleet
        .teams
        .iter()
        .find(|(_, cfg)| cfg.members.iter().any(|m| m == member))
        .map(|(name, cfg)| project_team(name, cfg))
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

    let fleet = load_fleet(home);
    if fleet.teams.contains_key(&name) {
        return serde_json::json!({"error": format!("team '{name}' already exists")});
    }
    let mut warnings: Vec<String> = Vec::new();
    // One-agent-one-team check
    for m in &members {
        if let Some(existing_team) = find_team_for_member(&fleet, m) {
            warnings.push(format!("member '{m}' already in team '{existing_team}'"));
            return serde_json::json!({"error": warnings[0]});
        }
    }

    let cfg = crate::fleet::TeamConfig {
        members,
        orchestrator,
        description,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
    };
    match crate::fleet::add_team_to_yaml(home, &name, &cfg) {
        Ok(true) => {
            let mut result = serde_json::json!({"status": "created", "name": name});
            if !warnings.is_empty() {
                result["warnings"] = serde_json::json!(warnings);
            }
            result
        }
        // Race: someone wrote the team between our check and write.
        Ok(false) => serde_json::json!({"error": format!("team '{name}' already exists")}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

pub fn delete(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n.to_string(),
        None => return serde_json::json!({"error": "missing 'name'"}),
    };
    match crate::fleet::remove_team_from_yaml(home, &name) {
        Ok(true) => serde_json::json!({"status": "deleted", "name": name}),
        Ok(false) => serde_json::json!({"error": format!("team '{name}' not found")}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

/// Return all teams as typed structs.
pub fn list_all(home: &Path) -> Vec<Team> {
    let fleet = load_fleet(home);
    fleet
        .teams
        .iter()
        .map(|(name, cfg)| project_team(name, cfg))
        .collect()
}

pub fn list(home: &Path) -> Value {
    let teams: Vec<Value> = list_all(home)
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

    let fleet = load_fleet(home);
    let Some(current) = fleet.teams.get(&name).cloned() else {
        return serde_json::json!({"error": format!("team '{name}' not found")});
    };

    // Block removing the orchestrator
    if let Some(ref orch) = current.orchestrator {
        if to_remove.contains(orch) {
            return serde_json::json!({
                "error": format!("cannot remove orchestrator '{orch}'; use update_team --orchestrator to reassign first")
            });
        }
    }
    // One-agent-one-team check on adds
    for m in &to_add {
        if let Some(existing) = find_team_for_member(&fleet, m) {
            if existing != name {
                return serde_json::json!({"error": format!("member '{m}' already in team '{existing}'")});
            }
        }
    }
    // Validate new orchestrator membership against the post-mutation
    // member set so reassign-then-add transactions don't bounce.
    let mut new_members = current.members.clone();
    for m in &to_add {
        if !new_members.contains(m) {
            new_members.push(m.clone());
        }
    }
    new_members.retain(|m| !to_remove.contains(m));
    let resolved_orch = if let Some(ref new_orch) = new_orchestrator {
        if !new_members.contains(new_orch) {
            return serde_json::json!({"error": format!("new orchestrator '{new_orch}' must be a current member")});
        }
        Some(new_orch.clone())
    } else {
        current.orchestrator.clone()
    };

    let cfg = crate::fleet::TeamConfig {
        members: new_members,
        orchestrator: resolved_orch,
        description: current.description.clone(),
        created_at: current.created_at.clone(),
    };
    match crate::fleet::update_team_in_yaml(home, &name, &cfg) {
        Ok(true) => serde_json::json!({"status": "updated", "name": name}),
        // Disappeared between load and write.
        Ok(false) => serde_json::json!({"error": format!("team '{name}' not found")}),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

/// Remove an instance from ALL teams. Auto-delete teams that become empty.
pub fn remove_member_from_all(home: &Path, instance_name: &str) {
    let fleet = load_fleet(home);
    let mut degraded_teams: Vec<String> = Vec::new();
    for (team_name, cfg) in &fleet.teams {
        let in_team = cfg.members.iter().any(|m| m == instance_name);
        let is_orch = cfg.orchestrator.as_deref() == Some(instance_name);
        if !in_team && !is_orch {
            continue;
        }
        let new_members: Vec<String> = cfg
            .members
            .iter()
            .filter(|m| *m != instance_name)
            .cloned()
            .collect();
        if new_members.is_empty() {
            // Last member leaving — drop the team entirely.
            let _ = crate::fleet::remove_team_from_yaml(home, team_name);
            continue;
        }
        let new_orch = if is_orch {
            degraded_teams.push(team_name.clone());
            None
        } else {
            cfg.orchestrator.clone()
        };
        let new_cfg = crate::fleet::TeamConfig {
            members: new_members,
            orchestrator: new_orch,
            description: cfg.description.clone(),
            created_at: cfg.created_at.clone(),
        };
        let _ = crate::fleet::update_team_in_yaml(home, team_name, &new_cfg);
    }
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

/// Resolve an assignee name: if it's a team, return the orchestrator.
/// Returns Ok(Some(orchestrator)) if team found, Ok(None) if not a team,
/// Err if team is degraded.
pub fn resolve_team_orchestrator(home: &Path, name: &str) -> Result<Option<String>, String> {
    let fleet = load_fleet(home);
    match fleet.teams.get(name) {
        Some(cfg) => match &cfg.orchestrator {
            Some(orch) => Ok(Some(orch.clone())),
            None => Err(format!(
                "team '{name}' is degraded (no orchestrator), cannot route task"
            )),
        },
        None => Ok(None), // not a team name
    }
}

/// Check if `caller` is the orchestrator of any team that `member` belongs to.
pub fn is_orchestrator_of(home: &Path, caller: &str, member: &str) -> bool {
    let fleet = load_fleet(home);
    fleet.teams.values().any(|cfg| {
        cfg.members.contains(&member.to_string()) && cfg.orchestrator.as_deref() == Some(caller)
    })
}

/// Get members of a team.
pub fn get_members(home: &Path, team_name: &str) -> Vec<String> {
    load_fleet(home)
        .teams
        .get(team_name)
        .map(|cfg| cfg.members.clone())
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

    /// Sprint 54 fleet-yaml unification: operator hand-editing fleet.yaml
    /// `teams:` block must surface immediately on next read — no separate
    /// reconcile step required. Locks the contract between fleet.rs
    /// `add_team_to_yaml` writer and `find_team_for` / `get_members`
    /// readers (both go through `FleetConfig::load`).
    #[test]
    fn fleet_yaml_seed_team_visible_on_first_read() {
        let home = tmp_home("seed_visible");
        let yaml = "teams:\n  ops:\n    members: [alice, bob]\n    orchestrator: alice\n";
        std::fs::write(home.join("fleet.yaml"), yaml).expect("write fleet.yaml");
        let members = get_members(&home, "ops");
        assert_eq!(members, vec!["alice", "bob"]);
        let listed = list(&home);
        assert_eq!(listed["teams"][0]["orchestrator"], "alice");
        std::fs::remove_dir_all(&home).ok();
    }
}
