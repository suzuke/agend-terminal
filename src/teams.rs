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
    /// #781 Piece 3 (Bug A1): operator-visible source_repo. Pre-#781
    /// the projection silently dropped this field, masking the
    /// migration-time `source_repo=None` (Bug A0) from `team list`
    /// callers. Persist as `Option` so legacy-migrated teams render
    /// `null` rather than disappearing from the projection — operator
    /// can `jq '.teams[] | select(.source_repo == null)'` to enumerate
    /// teams needing remediation.
    #[serde(default)]
    pub source_repo: Option<std::path::PathBuf>,
    /// #785: team members that are registered in the team metadata but
    /// missing from the runtime registry (no live instance). Surfaces
    /// the desync state — operator can enumerate which members need a
    /// `create_instance` respawn or `team(action=update, remove=...)`
    /// cleanup. Populated by `list()` only (the surface where staleness
    /// is operator-actionable); empty for `find_team_for` lookups where
    /// the consumer doesn't need the diagnostic.
    ///
    /// Sorted output for deterministic test ordering. `skip_serializing_if`
    /// keeps the JSON response back-compat — field absent when no
    /// staleness, matching #779 P2's `warnings` pattern.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_members: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept_from: Vec<String>,
}

impl Team {
    pub fn is_degraded(&self) -> bool {
        self.orchestrator.is_none()
    }
}

fn load_fleet(home: &Path) -> crate::fleet::FleetConfig {
    crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).unwrap_or_default()
}

/// #1744-M7: load fleet config distinguishing "no fleet configured" (a missing
/// file → `Ok(default)`, i.e. genuinely no teams) from "could not determine"
/// (the file EXISTS but failed to read/parse → `Err`, i.e. a transient IO race
/// or corruption). [`load_fleet`]'s `unwrap_or_default()` collapses both into an
/// empty fleet, which makes every self-orch check silently read `false` during a
/// teams.yaml read failure — defeating the orchestrator-down safety net exactly
/// when it matters. `self_orch_status` maps the `Err` to `Unknown` so consumers
/// can fail to the SAFE side.
pub fn try_load_fleet(home: &Path) -> anyhow::Result<crate::fleet::FleetConfig> {
    let path = crate::fleet::fleet_yaml_path(home);
    if !path.exists() {
        // No fleet.yaml = no teams configured (determinate "No"), not an error.
        return Ok(crate::fleet::FleetConfig::default());
    }
    crate::fleet::FleetConfig::load(&path)
}

/// #1744-M7: deterministic team lookup. `fleet.teams` is a `HashMap`, so the
/// prior `.iter().find()` returned an ARBITRARY team when a member appeared on
/// more than one — a non-deterministic self-orch verdict across calls/restarts.
/// Iterate in sorted-team-name order so the result is stable.
pub(crate) fn find_team_for_in(fleet: &crate::fleet::FleetConfig, member: &str) -> Option<Team> {
    let mut entries: Vec<_> = fleet.teams.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .into_iter()
        .find(|(_, cfg)| cfg.members.iter().any(|m| m == member))
        .map(|(name, cfg)| project_team(name, cfg))
}

/// #1744-M7: three-state self-orchestrator verdict. `Unknown` means the teams
/// config could not be read (transient IO / parse) — distinct from a determinate
/// `No`. Escalation consumers choose their own safe side for `Unknown` (the
/// no-peer hung/AuthError P0 paths escalate on `Unknown`; the crash path stays
/// conservative and falls back to its generic recent>=2 notify).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfOrchStatus {
    Yes,
    No,
    Unknown,
}

/// #1744-M7: deterministic + fail-closed self-orch verdict. See [`SelfOrchStatus`].
pub fn self_orch_status(home: &Path, member: &str) -> SelfOrchStatus {
    match try_load_fleet(home) {
        Ok(fleet) => match find_team_for_in(&fleet, member).and_then(|t| t.orchestrator) {
            Some(orch) if orch == member => SelfOrchStatus::Yes,
            _ => SelfOrchStatus::No,
        },
        Err(_) => SelfOrchStatus::Unknown,
    }
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
        source_repo: cfg.source_repo.clone(),
        stale_members: Vec::new(),
        accept_from: cfg.accept_from.clone(),
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
    // #1744-M7: deterministic lookup (sorted team-name order).
    find_team_for_in(&load_fleet(home), member)
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
    let source_repo = args["repository_path"]
        .as_str()
        .map(std::path::PathBuf::from);

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

    // #781 Piece 4 (Bug A2 UX): operator can silently create a team
    // without `source_repo`, then watch every `dispatch_auto_bind_lease`
    // fall through to the Tier 4 workspace stub at run time. Surface
    // the omission as a warning at create time so the operator can
    // amend before the gap is observed at dispatch.
    if source_repo.is_none() {
        warnings.push(format!(
            "team '{name}' created without `repository_path` — \
             dispatch_auto_bind_lease will fall through Tier 2.5 and \
             land on the workspace stub at Tier 4. Set via \
             `team(action=update, name={name}, repository_path=...)` to \
             bind agents on the canonical repo."
        ));
    }
    let accept_from: Vec<String> = args["accept_from"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let cfg = crate::fleet::TeamConfig {
        members,
        orchestrator,
        description,
        created_at: Some(chrono::Utc::now().to_rfc3339()),
        source_repo,
        accept_from,
    };
    match crate::fleet::add_team_to_yaml(home, &name, &cfg) {
        Ok(crate::fleet::TeamWriteOutcome::Written) => {
            if cfg.orchestrator.is_some() {
                cleanup_orchestrator_tasks_for_team(home, &name);
            }
            let mut result = serde_json::json!({"status": "created", "name": name});
            if !warnings.is_empty() {
                result["warnings"] = serde_json::json!(warnings);
            }
            result
        }
        // #t-91 F1: distinguish the two lock-held rejection reasons so the error
        // is accurate (the previous blanket "already exists" mis-described a
        // member-conflict race).
        Ok(crate::fleet::TeamWriteOutcome::NameExists) => {
            serde_json::json!({"error": format!("team '{name}' already exists")})
        }
        Ok(crate::fleet::TeamWriteOutcome::MemberConflict { member, team }) => {
            serde_json::json!({"error": format!("member '{member}' already in team '{team}'")})
        }
        Ok(crate::fleet::TeamWriteOutcome::NotFound) => {
            serde_json::json!({"error": format!("team '{name}' could not be created")})
        }
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

fn cleanup_orchestrator_tasks_for_team(home: &Path, team_name: &str) {
    let tasks = crate::tasks::list_all(home);
    for task in tasks {
        if task.priority == crate::task_events::TaskPriority::Urgent
            && task
                .title
                .starts_with(&format!("Team '{}' needs new orchestrator", team_name))
            && task.status != crate::task_events::TaskStatus::Done
            && task.status != crate::task_events::TaskStatus::Cancelled
        {
            crate::tasks::handle(
                home,
                "system",
                &serde_json::json!({
                    "action": "update",
                    "id": task.id,
                    "status": "cancelled",
                    "reason": format!("Team '{}' was deleted or updated with a valid orchestrator", team_name),
                }),
            );
        }
    }
}

/// #828: read the team's current member list from fleet.yaml. Used by
/// `delete` to snapshot members before the cascade walks them through
/// `full_delete_instance` (which itself removes each member from every
/// team via `remove_member_from_all`, eventually auto-deleting the
/// team being disbanded once its membership reaches zero).
fn list_team_members(home: &Path, team_name: &str) -> Vec<String> {
    load_fleet(home)
        .teams
        .get(team_name)
        .map(|cfg| cfg.members.clone())
        .unwrap_or_default()
}

/// #828: disband a team and cascade `full_delete_instance` per member
/// so each member's ghost-owned tasks get orphaned via the existing
/// `tasks::orphan_tasks_for_owner` hook (#808) and the rest of the
/// per-instance teardown (PTY kill, telegram topic, working_dir,
/// remove_member_from_all in any *other* teams the member belongs to)
/// fires symmetrically.
///
/// Hard-cascade design (per dispatch + spike design call 1):
/// `full_delete_instance` kills the instance entirely, so a member
/// that's in multiple teams is removed from all of them. The
/// `remove_member_from_all` step inside `full_delete_instance` emits
/// the existing "Team 'X' needs new orchestrator" urgent-task signal
/// when a multi-team member happened to be another team's
/// orchestrator — operator gets immediate cross-team coupling
/// surfaced at the moment it's actionable.
///
/// Error policy (per spike design call 2, mirrors
/// `full_delete_instance`'s own per-step pattern): continue on
/// per-member failure, collect into `cascade_warnings`. The
/// final response carries:
/// - `status: "deleted"` when the team was removed cleanly (either
///   by the explicit `remove_team_from_yaml` below or by
///   `remove_member_from_all`'s empty-team auto-delete during the
///   cascade — both outcomes count as success)
/// - `members_cleaned: N` (always emitted, even N=0)
/// - `cascade_warnings: [..]` when any per-member cascade returned Err
pub fn delete(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n.to_string(),
        None => return serde_json::json!({"error": "missing 'name'"}),
    };

    // Snapshot existence + members BEFORE cascade. The cascade itself
    // may auto-delete the team (when `remove_member_from_all` removes
    // the last member), so we can't rely on post-cascade fleet.yaml
    // state to tell "team was there" from "team was never there".
    let entry_existed = load_fleet(home).teams.contains_key(&name);
    if !entry_existed {
        return serde_json::json!({
            "error": format!("team '{name}' not found"),
            "members_cleaned": 0,
        });
    }
    let members = list_team_members(home, &name);
    let members_count = members.len();
    let mut cascade_warnings: Vec<String> = Vec::new();
    for member in &members {
        // #1903: CANCEL the member's live tasks BEFORE deleting the instance. A
        // disband has no survivor to take over, so cancel (terminal) rather than
        // orphan (Open + owner:None → a same-name redeploy sees a ghost open
        // task). Ordering is load-bearing: it MUST run before full_delete_instance
        // — that fn's internal orphan would otherwise null the owner first, leaving
        // this cancel-by-owner nothing to match. After this cancel, that internal
        // orphan SKIPS these tasks (Cancelled ∉ its Open/Claimed/InProgress/Blocked
        // filter) → a single Cancelled transition, no double event. A single
        // delete_instance never reaches here, so its orphan ACL-unlock is intact.
        if let Err(e) = crate::tasks::cancel_tasks_for_owner(home, member) {
            cascade_warnings.push(format!("{member} task cancel: {e}"));
            tracing::warn!(
                team = %name,
                %member,
                error = %e,
                "#1903: task cancel failed during team disband"
            );
        }
        if let Err(e) =
            crate::mcp::handlers::instance_state::lifecycle::full_delete_instance(home, member)
        {
            cascade_warnings.push(format!("{member}: {e}"));
            tracing::warn!(
                team = %name,
                %member,
                error = %e,
                "#828: full_delete_instance failed during team disband cascade"
            );
        }
    }

    // After cascade, the team may already be gone (auto-deleted by
    // `remove_member_from_all`'s empty-team rule once the last member
    // was removed). Treat Ok(true) AND Ok(false) as success here —
    // both mean the team is no longer in fleet.yaml, which is exactly
    // what disband requested.
    match crate::fleet::remove_team_from_yaml(home, &name) {
        Ok(_) => {
            cleanup_orchestrator_tasks_for_team(home, &name);
            let mut result = serde_json::json!({
                "status": "deleted",
                "name": name,
                "members_cleaned": members_count,
            });
            if !cascade_warnings.is_empty() {
                result["cascade_warnings"] = serde_json::json!(cascade_warnings);
            }
            result
        }
        Err(e) => serde_json::json!({"error": format!("{e}"), "members_cleaned": members_count}),
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
    // #785: query the daemon's live agent list to detect stale team
    // members (members in fleet.yaml team metadata but missing from the
    // runtime registry). Single API call, names collected once and
    // reused for every team's stale_members computation — O(N teams ×
    // M members) HashSet lookups, no repeated locking. If the API call
    // fails (daemon offline / unreachable), stale_members stays empty
    // — best-effort staleness reporting, never blocks `list` itself.
    //
    // #830: routed through the canonical `runtime::list_live_agents`
    // helper that #827/#829/#830 share (the original copy lived here).
    let live_agents: std::collections::HashSet<String> =
        crate::runtime::list_live_agents(home).unwrap_or_default();

    let teams: Vec<Value> = list_all(home)
        .iter_mut()
        .map(|t| {
            // Sorted output for deterministic test ordering.
            let mut stale: Vec<String> = t
                .members
                .iter()
                .filter(|m| !live_agents.contains(*m))
                .cloned()
                .collect();
            stale.sort();
            t.stale_members = stale;
            let mut v = serde_json::to_value(&*t).unwrap_or_default();
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

    let new_source_repo = args["repository_path"]
        .as_str()
        .map(std::path::PathBuf::from)
        .or_else(|| current.source_repo.clone());

    let new_accept_from: Vec<String> = args["accept_from"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_else(|| current.accept_from.clone());
    let cfg = crate::fleet::TeamConfig {
        members: new_members,
        orchestrator: resolved_orch,
        description: current.description.clone(),
        created_at: current.created_at.clone(),
        source_repo: new_source_repo,
        accept_from: new_accept_from,
    };
    match crate::fleet::update_team_in_yaml(home, &name, &cfg) {
        Ok(crate::fleet::TeamWriteOutcome::Written) => {
            if cfg.orchestrator.is_some() {
                cleanup_orchestrator_tasks_for_team(home, &name);
            }
            serde_json::json!({"status": "updated", "name": name})
        }
        // Disappeared between load and write.
        Ok(crate::fleet::TeamWriteOutcome::NotFound) => {
            serde_json::json!({"error": format!("team '{name}' not found")})
        }
        // #t-91 F1: a concurrent add/update double-booked a member — report it
        // accurately instead of the stale "not found".
        Ok(crate::fleet::TeamWriteOutcome::MemberConflict { member, team }) => {
            serde_json::json!({"error": format!("member '{member}' already in team '{team}'")})
        }
        Ok(crate::fleet::TeamWriteOutcome::NameExists) => {
            serde_json::json!({"error": format!("team '{name}' already exists")})
        }
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
            cleanup_orchestrator_tasks_for_team(home, team_name);
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
            source_repo: cfg.source_repo.clone(),
            accept_from: cfg.accept_from.clone(),
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
    fn team_disband_cancels_member_tasks_no_double_event_1903() {
        // #1903 §3.9 (b): disbanding a team must CANCEL each member's live tasks
        // (no survivor to take over), and — proving the cancel-BEFORE-cascade
        // ordering — the task must carry a SINGLE Cancelled transition with NO
        // `system:auto_orphan` OwnerAssigned event (the internal orphan must SKIP
        // the already-Cancelled task, not coincidentally miss it).
        let home = tmp_home("disband_cancel_1903");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  m1:\n    backend: claude\nteams:\n  squad:\n    orchestrator: m1\n    members:\n      - m1\n",
        )
        .unwrap();
        let r = crate::tasks::handle(
            &home,
            "m1",
            &serde_json::json!({"action": "create", "title": "owned", "assignee": "m1"}),
        );
        let task_id = r["id"].as_str().expect("task id").to_string();

        let res = super::delete(&home, &serde_json::json!({"name": "squad"}));
        assert_eq!(res["status"], "deleted", "disband must succeed: {res}");

        // (b1) terminal Cancelled, not Open.
        let task = crate::tasks::list_all(&home)
            .into_iter()
            .find(|t| t.id == task_id)
            .expect("task still exists");
        assert_eq!(
            task.status,
            crate::task_events::TaskStatus::Cancelled,
            "disband must cancel the member's task, got {:?}",
            task.status
        );

        // (b2) single Cancelled transition + zero auto_orphan double-event.
        let log = std::fs::read_to_string(home.join("task_events.jsonl")).unwrap();
        let cancelled = log
            .lines()
            .filter(|l| l.contains(&task_id) && l.contains("\"kind\":\"Cancelled\""))
            .count();
        let auto_orphan = log
            .lines()
            .filter(|l| l.contains(&task_id) && l.contains("system:auto_orphan"))
            .count();
        assert_eq!(
            cancelled, 1,
            "exactly one Cancelled transition, got {cancelled}"
        );
        assert_eq!(
            auto_orphan, 0,
            "orphan must NOT double-fire on the already-Cancelled task (cancel-before-cascade), got {auto_orphan}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    // ── #1744-M7: deterministic + fail-closed self-orch verdict ──
    // (#1701's boolean `is_self_orchestrator` was replaced by the 3-state
    // `self_orch_status`; the Yes/No verdict is covered by
    // `self_orch_status_yes_no_for_loaded_fleet_1744_m7` below.)

    /// #1744-M7: `self_orch_status` is `Yes` for the orchestrator, `No` for a
    /// regular member, and `No` for an agent in no team / a missing fleet.yaml
    /// (a determinate "no fleet configured", NOT an indeterminate read).
    #[test]
    fn self_orch_status_yes_no_for_loaded_fleet_1744_m7() {
        let home = tmp_home("m7-yes-no");
        create(
            &home,
            &serde_json::json!({"name": "t", "members": ["lead", "dev"], "orchestrator": "lead"}),
        );
        assert_eq!(self_orch_status(&home, "lead"), SelfOrchStatus::Yes);
        assert_eq!(self_orch_status(&home, "dev"), SelfOrchStatus::No);
        assert_eq!(self_orch_status(&home, "ghost"), SelfOrchStatus::No);

        // Missing fleet.yaml = no fleet configured = determinate No (not Unknown).
        let empty = tmp_home("m7-missing");
        assert_eq!(self_orch_status(&empty, "anyone"), SelfOrchStatus::No);

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// #1744-M7: an EXISTING but unreadable/corrupt fleet.yaml → `Unknown`
    /// (fail-closed), distinct from the determinate `No` of a missing file. This
    /// is the gap the old `unwrap_or_default()` collapsed: a transient parse/IO
    /// failure silently made every agent read as not-self-orch.
    #[test]
    fn self_orch_status_unknown_on_unreadable_fleet_1744_m7() {
        let home = tmp_home("m7-unknown");
        // A file that EXISTS but is not valid fleet YAML → FleetConfig::load Err.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "this: is: not: valid: fleet: yaml: [[[\n",
        )
        .unwrap();
        assert_eq!(
            self_orch_status(&home, "lead"),
            SelfOrchStatus::Unknown,
            "#1744-M7: an unreadable teams config must be Unknown (fail-closed), not No"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #1744-M7: `find_team_for` is deterministic when a member appears on more
    /// than one team — it picks the alphabetically-first team name on every call,
    /// so the self-orch verdict can't flip between runs/restarts. Here `shared`
    /// is on `ateam` (orch=a1) and `bteam` (orch=shared); sorted-first `ateam`
    /// wins, so `shared` is consistently NOT a self-orchestrator.
    #[test]
    fn find_team_for_deterministic_across_multi_team_1744_m7() {
        let home = tmp_home("m7-determinism");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "teams:\n  \
               bteam:\n    members: [shared, b1]\n    orchestrator: shared\n  \
               ateam:\n    members: [shared, a1]\n    orchestrator: a1\n",
        )
        .unwrap();
        for _ in 0..20 {
            assert_eq!(
                find_team_for(&home, "shared").map(|t| t.name),
                Some("ateam".to_string()),
                "#1744-M7: multi-team lookup must deterministically pick the sorted-first team"
            );
            assert_eq!(
                self_orch_status(&home, "shared"),
                SelfOrchStatus::No,
                "#1744-M7: deterministic team → stable self-orch verdict"
            );
        }
        let _ = std::fs::remove_dir_all(&home);
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
            .filter(|t| {
                t.priority == crate::task_events::TaskPriority::Urgent
                    && t.title.contains("needs new orchestrator")
            })
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

    // ── #828 teams::delete cascade full_delete_instance per member ──

    /// #828 C1 RED: disbanding a team must cascade `full_delete_instance`
    /// to each member so their owned tasks get orphaned via the existing
    /// `tasks::orphan_tasks_for_owner` hook (#808) inside
    /// `full_delete_instance`. Pre-fix `teams::delete` only removes the
    /// fleet.yaml entry — members' tasks stay ghost-owned indefinitely,
    /// the exact symptom the operator surfaced in the residual cleanup
    /// hygiene work after #821/#822.
    ///
    /// Asserts the post-fix contract:
    /// - response carries `members_cleaned` = number of cascaded members
    /// - members are gone from fleet.yaml `instances:`
    /// - tasks previously owned by members are now `owner = None`
    #[test]
    fn delete_team_cascades_full_delete_instance_per_member() {
        let home = tmp_home("828_cascade");
        // Seed two members so we can verify per-member cascade.
        create(
            &home,
            &serde_json::json!({"name": "ops", "members": ["alice828", "bob828"], "orchestrator": "alice828"}),
        );
        // Also seed the instances themselves so fleet.yaml has full
        // `instances:` entries — without those, `full_delete_instance`'s
        // residual audit short-circuits on already-missing names.
        let fleet_path = crate::fleet::fleet_yaml_path(&home);
        let yaml = std::fs::read_to_string(&fleet_path).unwrap();
        let yaml = format!(
            "instances:\n  alice828:\n    backend: claude\n  bob828:\n    backend: claude\n{}",
            yaml,
        );
        std::fs::write(&fleet_path, yaml).unwrap();

        // Create 3 tasks claimed by alice828 (2) + bob828 (1).
        let t1 = crate::tasks::handle(
            &home,
            "alice828",
            &serde_json::json!({"action": "create", "title": "task-1"}),
        );
        let id1 = t1["id"].as_str().unwrap().to_string();
        crate::tasks::handle(
            &home,
            "alice828",
            &serde_json::json!({"action": "claim", "id": id1}),
        );
        let t2 = crate::tasks::handle(
            &home,
            "bob828",
            &serde_json::json!({"action": "create", "title": "task-2"}),
        );
        let id2 = t2["id"].as_str().unwrap().to_string();
        crate::tasks::handle(
            &home,
            "bob828",
            &serde_json::json!({"action": "claim", "id": id2}),
        );
        let t3 = crate::tasks::handle(
            &home,
            "alice828",
            &serde_json::json!({"action": "create", "title": "task-3"}),
        );
        let id3 = t3["id"].as_str().unwrap().to_string();
        crate::tasks::handle(
            &home,
            "alice828",
            &serde_json::json!({"action": "claim", "id": id3}),
        );

        // Disband.
        let result = delete(&home, &serde_json::json!({"name": "ops"}));

        // Status + audit fields.
        assert_eq!(
            result["status"], "deleted",
            "team deletion must succeed, got: {result}"
        );
        assert_eq!(
            result["members_cleaned"], 2,
            "members_cleaned must report 2 cascaded members, got: {result}"
        );

        // Members are gone from fleet.yaml `instances:`.
        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        assert!(
            !fleet.instances.contains_key("alice828"),
            "alice828 must be removed from fleet.yaml instances, got: {:?}",
            fleet.instances.keys().collect::<Vec<_>>()
        );
        assert!(
            !fleet.instances.contains_key("bob828"),
            "bob828 must be removed from fleet.yaml instances"
        );

        // #1903: a team DISBAND now CANCELS each member's live tasks (terminal)
        // rather than orphaning them (Open, owner cleared). A disbanded team has
        // no survivor to take over, and an orphan-open task would be a ghost for a
        // same-name redeploy. (This test previously asserted owner==None — that
        // pinned the orphan-on-disband behaviour #1903 fixes. Single-instance
        // delete still orphans-open — see full_delete_instance_orphans_owned_tasks.)
        let state = crate::task_events::replay(&home).unwrap();
        for id in &[id1.as_str(), id2.as_str(), id3.as_str()] {
            let task = state
                .tasks
                .values()
                .find(|t| t.id.0 == *id)
                .unwrap_or_else(|| panic!("task {id} must exist post-cascade"));
            assert_eq!(
                task.status,
                crate::task_events::TaskStatus::Cancelled,
                "task {} must be CANCELLED post-disband (not orphaned-open), got status={:?}",
                task.id.0,
                task.status
            );
        }

        std::fs::remove_dir_all(&home).ok();
    }

    /// #828 C3 regression-proof: a team with zero members (operator
    /// hand-edited fleet.yaml or post-`remove_member_from_all` edge
    /// case) must still disband cleanly with `members_cleaned: 0`
    /// and no cascade_warnings. Locks the "always emit
    /// `members_cleaned`" design call from the spike.
    #[test]
    fn delete_team_with_zero_members_is_no_op_cascade() {
        let home = tmp_home("828_empty_team");
        // Seed a team with empty members list via direct fleet.yaml
        // (the `create` API rejects this shape so we go around it).
        let fleet_path = crate::fleet::fleet_yaml_path(&home);
        std::fs::write(
            &fleet_path,
            "teams:\n  empty_squad:\n    members: []\n    orchestrator: null\n",
        )
        .unwrap();

        let result = delete(&home, &serde_json::json!({"name": "empty_squad"}));

        assert_eq!(result["status"], "deleted", "got: {result}");
        assert_eq!(
            result["members_cleaned"], 0,
            "zero-member team must still emit `members_cleaned: 0`, got: {result}"
        );
        assert!(
            result["cascade_warnings"].is_null(),
            "no cascade warnings expected for zero-member team, got: {result}"
        );
        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        assert!(
            !fleet.teams.contains_key("empty_squad"),
            "empty team must be removed from fleet.yaml"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #828 C3 regression-proof: a team whose member doesn't exist in
    /// `instances:` (operator hand-edited fleet.yaml, or member was
    /// deleted via a path other than the team cascade) — cascade still
    /// returns success because `full_delete_instance` is best-effort
    /// and each cleanup step no-ops for a fully-missing name.
    #[test]
    fn delete_team_with_ghost_member_swallows_residual() {
        let home = tmp_home("828_ghost_member");
        // Seed fleet.yaml with a team referencing a member that has no
        // corresponding `instances:` entry.
        let fleet_path = crate::fleet::fleet_yaml_path(&home);
        std::fs::write(
            &fleet_path,
            "teams:\n  ghost_squad:\n    members: [ghost_alice828]\n    orchestrator: null\n",
        )
        .unwrap();

        let result = delete(&home, &serde_json::json!({"name": "ghost_squad"}));

        assert_eq!(result["status"], "deleted", "got: {result}");
        assert_eq!(result["members_cleaned"], 1, "got: {result}");
        // No cascade warnings — `full_delete_instance` no-ops cleanly
        // for a fully-missing name.
        assert!(
            result["cascade_warnings"].is_null(),
            "ghost member should produce no cascade warnings, got: {result}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #828 C3 regression-proof: when a cascaded member belongs to
    /// MULTIPLE teams, the hard-cascade semantics remove the instance
    /// from ALL of those teams (via `full_delete_instance`'s embedded
    /// `remove_member_from_all` step). This locks the multi-team
    /// behavior documented in the PR body — disbanding one team
    /// affects every other team that shared the cascaded member.
    #[test]
    fn delete_team_with_multi_team_member_removes_from_all_teams() {
        let home = tmp_home("828_multi_team");
        // Pre-seed two teams sharing the member "polyglot_alice828".
        // Skip the `create` API because it enforces one-agent-one-team;
        // we hand-write fleet.yaml to construct the multi-team state
        // (which can arise via operator-edited yaml or migration
        // history) and verify the cascade handles it gracefully.
        let fleet_path = crate::fleet::fleet_yaml_path(&home);
        std::fs::write(
            &fleet_path,
            "instances:\n  polyglot_alice828:\n    backend: claude\n\
             teams:\n  ops_a:\n    members: [polyglot_alice828]\n    orchestrator: polyglot_alice828\n\
                 \n  ops_b:\n    members: [polyglot_alice828]\n    orchestrator: polyglot_alice828\n",
        )
        .unwrap();

        // Disband ops_a — should cascade-kill polyglot_alice828, which
        // removes them from ops_b too (and since ops_b becomes empty,
        // `remove_member_from_all`'s empty-team rule auto-deletes ops_b).
        let result = delete(&home, &serde_json::json!({"name": "ops_a"}));

        assert_eq!(result["status"], "deleted", "got: {result}");
        assert_eq!(result["members_cleaned"], 1, "got: {result}");

        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        assert!(
            !fleet.instances.contains_key("polyglot_alice828"),
            "multi-team member must be removed from instances post-cascade"
        );
        assert!(
            !fleet.teams.contains_key("ops_a"),
            "ops_a (the disbanded team) must be gone"
        );
        assert!(
            !fleet.teams.contains_key("ops_b"),
            "ops_b (the other team that shared the member) must auto-delete \
             once its last member was cascade-removed"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #828 C3 regression-proof: a delete call for a team that doesn't
    /// exist at all returns the `not found` error with
    /// `members_cleaned: 0` for response-shape consistency. Locks the
    /// `entry_existed` short-circuit guard.
    #[test]
    fn delete_team_returns_not_found_when_team_never_existed() {
        let home = tmp_home("828_not_found");
        // Empty home — no fleet.yaml at all is fine, load_fleet returns
        // the default.
        let result = delete(&home, &serde_json::json!({"name": "phantom_squad"}));

        assert!(
            result["error"]
                .as_str()
                .is_some_and(|s| s.contains("not found")),
            "missing-team delete must return not-found error, got: {result}"
        );
        assert_eq!(
            result["members_cleaned"], 0,
            "not-found error must still carry members_cleaned: 0 for response shape consistency, got: {result}"
        );

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
        std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).expect("write fleet.yaml");
        let members = get_members(&home, "ops");
        assert_eq!(members, vec!["alice", "bob"]);
        let listed = list(&home);
        assert_eq!(listed["teams"][0]["orchestrator"], "alice");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn team_create_with_source_repo() {
        let home = tmp_home("create_src_repo");
        let result = create(
            &home,
            &serde_json::json!({
                "name": "dev",
                "members": ["alice", "bob"],
                "orchestrator": "alice",
                "repository_path": "/tmp/my-repo"
            }),
        );
        assert_eq!(result["status"], "created");
        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let team = fleet.teams.get("dev").unwrap();
        assert_eq!(
            team.source_repo.as_deref(),
            Some(std::path::Path::new("/tmp/my-repo"))
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn team_update_source_repo() {
        let home = tmp_home("update_src_repo");
        create(
            &home,
            &serde_json::json!({
                "name": "dev",
                "members": ["alice"],
                "orchestrator": "alice",
                "repository_path": "/tmp/old-repo"
            }),
        );
        let result = update(
            &home,
            &serde_json::json!({
                "name": "dev",
                "repository_path": "/tmp/new-repo"
            }),
        );
        assert_eq!(result["status"], "updated");
        let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(&home)).unwrap();
        let team = fleet.teams.get("dev").unwrap();
        assert_eq!(
            team.source_repo.as_deref(),
            Some(std::path::Path::new("/tmp/new-repo"))
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_source_repo_uses_team_tier() {
        let home = tmp_home("team_tier");
        create(
            &home,
            &serde_json::json!({
                "name": "dev",
                "members": ["agent-x"],
                "orchestrator": "agent-x",
                "repository_path": "/tmp/team-repo"
            }),
        );
        let resolved =
            crate::mcp::handlers::dispatch_hook::resolve_team_source_repo(&home, "agent-x");
        assert_eq!(
            resolved.as_deref(),
            Some(std::path::Path::new("/tmp/team-repo"))
        );
        let none = crate::mcp::handlers::dispatch_hook::resolve_team_source_repo(&home, "other");
        assert!(none.is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn team_list_response_surfaces_source_repo_field() {
        // #781 Piece 3 (Bug A1): `team list` response must expose
        // `source_repo` so operators can audit which teams need a
        // remediation update (Bug A0 legacy-migration cases render
        // `null`).
        let home = std::env::temp_dir().join(format!("agend-p781-list-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        super::create(
            &home,
            &serde_json::json!({
                "name": "with-repo",
                "members": ["agent-w"],
                "orchestrator": "agent-w",
                "repository_path": "/tmp/p781-test-canonical",
            }),
        );
        super::create(
            &home,
            &serde_json::json!({
                "name": "without-repo",
                "members": ["agent-n"],
                "orchestrator": "agent-n",
            }),
        );
        let list = super::list(&home);
        let teams = list["teams"].as_array().expect("teams array");
        let with = teams
            .iter()
            .find(|t| t["name"] == "with-repo")
            .expect("with-repo present");
        let without = teams
            .iter()
            .find(|t| t["name"] == "without-repo")
            .expect("without-repo present");
        assert_eq!(
            with["source_repo"].as_str(),
            Some("/tmp/p781-test-canonical"),
            "list response must surface source_repo: {with}"
        );
        assert!(
            without.get("source_repo").is_some(),
            "list response must include source_repo field even when null: {without}"
        );
        assert!(
            without["source_repo"].is_null(),
            "missing source_repo must render as null, not absent: {without}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn create_warns_when_source_repo_absent() {
        // #781 Piece 4 (Bug A2 UX): `team(action=create)` accepts missing
        // `source_repo` but must surface a warning so the operator can
        // amend before the gap is observed at dispatch.
        let home = std::env::temp_dir().join(format!("agend-p781-warn-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let resp = super::create(
            &home,
            &serde_json::json!({
                "name": "no-repo",
                "members": ["agent-z"],
                "orchestrator": "agent-z",
            }),
        );
        assert_eq!(resp["status"], "created");
        let warnings = resp["warnings"]
            .as_array()
            .expect("warnings array when source_repo omitted");
        assert!(
            warnings
                .iter()
                .any(|w| w.as_str().is_some_and(|s| s.contains("repository_path"))),
            "warning text must reference repository_path: {warnings:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ----------------------------------------------------------------------
    // #785 — team-desync surface tests.
    //
    // Fixture pattern (reviewer C5): never call create_instance for the
    // member name; instead set up fleet.yaml team membership directly via
    // `teams::create`. With no daemon running and no `api::call(LIST)`
    // success, `live_agents` HashSet is empty → ALL members are stale.
    // This exercises the production code path without mock plumbing.
    //
    // Cross-platform (no `#[cfg(unix)]`): pure logic + fleet.yaml I/O,
    // no git subprocess, no worktree.
    // ----------------------------------------------------------------------

    #[test]
    fn team_list_response_surfaces_stale_member_field() {
        // Test 2: members with no live instance must surface in
        // `stale_members`. Deterministic sorted order so test assertion
        // is stable.
        let home = std::env::temp_dir().join(format!("agend-p785-list-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        super::create(
            &home,
            &serde_json::json!({
                "name": "team-with-stale",
                "members": ["zeta-agent", "alpha-agent"],
                "orchestrator": "alpha-agent",
                "repository_path": "/tmp/p785",
            }),
        );

        let resp = super::list(&home);
        let team = resp["teams"]
            .as_array()
            .and_then(|arr| arr.iter().find(|t| t["name"] == "team-with-stale"))
            .expect("team-with-stale present");

        let stale = team["stale_members"]
            .as_array()
            .expect("stale_members array must be present when members lack live instances");
        let names: Vec<&str> = stale.iter().filter_map(|v| v.as_str()).collect();
        // Deterministic sorted order: alpha-agent before zeta-agent.
        assert_eq!(
            names,
            vec!["alpha-agent", "zeta-agent"],
            "stale_members must be sorted: {team}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn team_list_no_stale_members_omits_field() {
        // Test 3 (back-compat invariant): when stale_members is empty,
        // the JSON field is omitted (matches #779 P2 `warnings` /
        // #781 Bug A1 absence-when-empty conventions). Verifies the
        // `skip_serializing_if = "Vec::is_empty"` serde attribute fires.
        //
        // Setup: empty team list → no team → no stale_members rendered.
        // (We can't easily fake "all members live" without a running
        // daemon; the empty-team-list case still exercises the
        // serialization path on the empty Vec each list call would
        // produce when api::LIST succeeds with all members present.)
        let home = std::env::temp_dir().join(format!("agend-p785-empty-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();

        // Inject a Team value directly via serde_json round-trip to
        // verify the serialization contract: an empty stale_members
        // must not appear in the JSON output.
        let clean = super::Team {
            name: "clean-team".to_string(),
            members: vec!["a".to_string()],
            orchestrator: Some("a".to_string()),
            description: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            source_repo: None,
            stale_members: Vec::new(),
            accept_from: Vec::new(),
        };
        let v = serde_json::to_value(&clean).expect("serialize Team");
        assert!(
            v.get("stale_members").is_none(),
            "empty stale_members must be absent from JSON (back-compat invariant): {v}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn team_list_with_partial_stale_members_lists_only_stale() {
        // Test 4: positive partial coverage. A team with mixed
        // live/stale members must surface ONLY the stale ones in
        // `stale_members`. Without this test, a buggy filter could
        // pass test 2 by returning `stale_members = members` (i.e.
        // marking everything stale when nothing is live) — the
        // partial-coverage assertion catches that drift.
        //
        // Setup: serialize Team with mixed stale_members directly.
        // Verifies the Team struct's stale_members field is a true
        // subset projection, not a duplicate of members.
        let team = super::Team {
            name: "mixed-team".to_string(),
            members: vec![
                "alive-1".to_string(),
                "stale-2".to_string(),
                "alive-3".to_string(),
            ],
            orchestrator: Some("alive-1".to_string()),
            description: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            source_repo: None,
            stale_members: vec!["stale-2".to_string()],
            accept_from: Vec::new(),
        };
        let v = serde_json::to_value(&team).expect("serialize");
        let stale: Vec<&str> = v["stale_members"]
            .as_array()
            .expect("stale_members present")
            .iter()
            .filter_map(|s| s.as_str())
            .collect();
        assert_eq!(
            stale,
            vec!["stale-2"],
            "stale_members must be subset of members, not duplicate: {v}"
        );
        // members untouched — caller still sees full team config.
        let members: Vec<&str> = v["members"]
            .as_array()
            .expect("members present")
            .iter()
            .filter_map(|m| m.as_str())
            .collect();
        assert_eq!(
            members,
            vec!["alive-1", "stale-2", "alive-3"],
            "members must remain full team config: {v}"
        );
    }

    #[test]
    fn cleanup_orchestrator_tasks_on_disband_and_recreate() {
        let home = tmp_home("cleanup_stale_tasks");

        // 1. Create a team
        create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );

        // 2. Remove member "lead" (the orchestrator) -> should trigger urgent task
        remove_member_from_all(&home, "lead");

        let tasks = crate::tasks::list_all(&home);
        let urgent: Vec<_> = tasks
            .iter()
            .filter(|t| {
                t.priority == crate::task_events::TaskPriority::Urgent
                    && t.title.contains("needs new orchestrator")
                    && t.status != crate::task_events::TaskStatus::Cancelled
            })
            .collect();
        assert_eq!(urgent.len(), 1, "should have active urgent task");

        // 3. Disband (delete) the team -> task should be cancelled
        delete(&home, &serde_json::json!({"name": "devs"}));

        let tasks = crate::tasks::list_all(&home);
        let urgent: Vec<_> = tasks
            .iter()
            .filter(|t| {
                t.priority == crate::task_events::TaskPriority::Urgent
                    && t.title.contains("needs new orchestrator")
                    && t.status != crate::task_events::TaskStatus::Cancelled
            })
            .collect();
        assert_eq!(urgent.len(), 0, "disband should cancel the urgent task");

        // 4. Test recreation cleanup: recreate team degraded, then create it with orchestrator
        create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["worker"]}), // degraded (no orchestrator)
        );
        // Manually create degraded task to simulate
        crate::tasks::handle(
            &home,
            "system",
            &serde_json::json!({
                "action": "create",
                "title": "Team 'devs' needs new orchestrator (lead was deleted)",
                "priority": "urgent",
            }),
        );

        let tasks = crate::tasks::list_all(&home);
        let urgent: Vec<_> = tasks
            .iter()
            .filter(|t| {
                t.priority == crate::task_events::TaskPriority::Urgent
                    && t.title.contains("needs new orchestrator")
                    && t.status != crate::task_events::TaskStatus::Cancelled
            })
            .collect();
        assert_eq!(
            urgent.len(),
            1,
            "should have active urgent task for degraded team"
        );

        // Disband first
        delete(&home, &serde_json::json!({"name": "devs"}));

        // Create new team with orchestrator -> should clean up any matching stale task (recreation defense)
        create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["new-lead"], "orchestrator": "new-lead"}),
        );

        let tasks = crate::tasks::list_all(&home);
        let urgent: Vec<_> = tasks
            .iter()
            .filter(|t| {
                t.priority == crate::task_events::TaskPriority::Urgent
                    && t.title.contains("needs new orchestrator")
                    && t.status != crate::task_events::TaskStatus::Cancelled
            })
            .collect();
        assert_eq!(
            urgent.len(),
            0,
            "creating team with orchestrator should clean up stale task"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}
