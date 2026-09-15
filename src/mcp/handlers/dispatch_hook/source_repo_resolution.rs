use std::path::{Path, PathBuf};

/// Resolve source_repo from the agent's team configuration.
///
/// #781 Piece 5 (defensive logging): the prior `.ok()?` swallowed
/// FleetConfig::load errors silently — a malformed fleet.yaml or
/// transient I/O fault dropped Tier 2.5 to None with zero diagnostics,
/// making post-mortem investigation harder. The defensive branches
/// below surface the actual error class (load failure vs no team
/// match vs team match without `source_repo`) so operators can
/// distinguish "Bug A0 legacy-migration case" (team matched but
/// source_repo None) from "team membership setup gap".
pub(crate) fn resolve_team_source_repo(home: &Path, agent: &str) -> Option<PathBuf> {
    let fleet = match crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                %agent,
                home = %home.display(),
                error = %e,
                "Tier 2.5 resolution skipped: fleet.yaml load failed — \
                 dispatch will fall through to tier 3 (working_directory) or \
                 tier 4 (workspace stub)"
            );
            return None;
        }
    };
    for cfg in fleet.teams.values() {
        if cfg.members.contains(&agent.to_string()) {
            if cfg.source_repo.is_none() {
                tracing::warn!(
                    %agent,
                    "Tier 2.5 team match but `source_repo` is None — \
                     likely legacy migration from teams.json (Bug A0, see #781). \
                     Operator must run `team update name=<team> source_repo=<canonical>` \
                     to escape the workspace stub fallback at tier 4"
                );
            }
            return cfg.source_repo.clone();
        }
    }
    tracing::debug!(
        %agent,
        teams_searched = fleet.teams.len(),
        "Tier 2.5: no team membership found for agent — falling through to tier 3/4"
    );
    None
}
