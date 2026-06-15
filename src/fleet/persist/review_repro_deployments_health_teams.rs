//! Repro batch `deployments-health-teams` — Finding 3
//! ("teams create one-agent-one-team is a TOCTOU").
//!
//! `teams::create` checks the one-agent-one-team invariant on a `load_fleet`
//! SNAPSHOT taken before the write, but the lock-held write boundary
//! `add_team_to_yaml` re-checks only the team-NAME key
//! (`teams.contains_key(&key)`) and never inspects existing teams' members.
//! So the exclusivity check is never re-validated under the lock — two
//! concurrent creates of different team names sharing one agent both pass
//! the stale snapshot and both write, landing the agent in two teams.
//!
//! This drives the lock-held write seam directly and deterministically: an
//! agent already in team `alpha` must not be writable into a second team
//! `beta`. The suggested fix is to "validate exclusivity inside the mutate
//! closure under the lock" — i.e. exactly here. RED now (the write succeeds
//! and the agent ends up in BOTH teams), GREEN once the closure enforces
//! one-agent-one-team.

#![allow(clippy::unwrap_used)]

use super::add_team_to_yaml;
use crate::fleet::{fleet_yaml_path, FleetConfig, TeamConfig};

fn tmp_home(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-teams-toctou-dht-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&dir).expect("create tmp home");
    dir
}

fn team_with_member(member: &str) -> TeamConfig {
    TeamConfig {
        members: vec![member.to_string()],
        orchestrator: None,
        description: None,
        created_at: Some("2026-06-14T00:00:00Z".to_string()),
        source_repo: None,
        accept_from: Vec::new(),
    }
}

#[test]
fn add_team_to_yaml_enforces_one_agent_one_team_deployments_health_teams() {
    let home = tmp_home("excl");
    std::fs::write(fleet_yaml_path(&home), "teams: {}\n").expect("seed fleet.yaml");

    // Establish team 'alpha' owning member 'shared'.
    let inserted_alpha = add_team_to_yaml(&home, "alpha", &team_with_member("shared"))
        .expect("add_team_to_yaml alpha must not error");
    assert!(inserted_alpha, "team 'alpha' should be newly inserted");

    // Now attempt to add a DIFFERENT team 'beta' that also claims 'shared'.
    // The team-name key is free, so the lock-held closure's name-only re-check
    // passes — the pre-fix code happily writes 'beta', double-booking 'shared'.
    let _ = add_team_to_yaml(&home, "beta", &team_with_member("shared"));

    // Reload from disk and count how many teams 'shared' belongs to. The
    // one-agent-one-team invariant requires at most one.
    let cfg = FleetConfig::load(&fleet_yaml_path(&home)).expect("reload fleet.yaml");
    let teams_with_shared: Vec<&String> = cfg
        .teams
        .iter()
        .filter(|(_, t)| t.members.iter().any(|m| m == "shared"))
        .map(|(name, _)| name)
        .collect();

    assert!(
        teams_with_shared.len() <= 1,
        "one-agent-one-team must be enforced inside the lock-held mutate closure: \
         member 'shared' ended up in {} teams ({:?}); add_team_to_yaml re-checks only \
         the team-name key, not member exclusivity",
        teams_with_shared.len(),
        teams_with_shared
    );

    std::fs::remove_dir_all(&home).ok();
}
