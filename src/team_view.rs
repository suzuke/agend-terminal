//! App-owned, fail-closed view of team authority and live process identity.

use crate::fleet::{FleetConfig, TeamConfig};
use crate::types::InstanceRef;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TeamViewStatus {
    Fresh,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeadBadge {
    None,
    Lead,
    Uncertain,
}

#[derive(Debug, Clone)]
struct TeamRecord {
    orchestrator: String,
}

/// Immutable snapshot consumed by all render surfaces. The app owns one of
/// these and replaces it outside the ratatui draw closure.
#[derive(Debug, Clone)]
pub(crate) struct TeamView {
    status: TeamViewStatus,
    teams: HashMap<String, TeamRecord>,
    member_team: HashMap<String, String>,
    current_roster: HashMap<String, InstanceRef>,
    last_roster: HashMap<String, InstanceRef>,
    roster_live: bool,
}

impl TeamView {
    pub(crate) fn empty() -> Self {
        Self {
            status: TeamViewStatus::Unavailable,
            teams: HashMap::new(),
            member_team: HashMap::new(),
            current_roster: HashMap::new(),
            last_roster: HashMap::new(),
            roster_live: false,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn status(&self) -> TeamViewStatus {
        self.status
    }

    pub(crate) fn from_fleet(
        config: FleetConfig,
        live_roster: Option<HashMap<String, InstanceRef>>,
    ) -> Self {
        let Some((teams, member_team)) = validate_teams(&config.teams) else {
            return Self {
                status: TeamViewStatus::Unavailable,
                teams: HashMap::new(),
                member_team: HashMap::new(),
                current_roster: HashMap::new(),
                last_roster: HashMap::new(),
                roster_live: false,
            };
        };
        let roster_live = live_roster.is_some();
        let current_roster = live_roster.unwrap_or_default();
        let status = if teams.is_empty() || roster_live {
            TeamViewStatus::Fresh
        } else {
            TeamViewStatus::Unavailable
        };
        Self {
            status,
            teams,
            member_team,
            last_roster: current_roster.clone(),
            current_roster,
            roster_live,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn load(home: &Path, live_roster: Option<HashMap<String, InstanceRef>>) -> Self {
        Self::load_path(&crate::fleet::fleet_yaml_path(home), live_roster, None)
    }

    #[allow(dead_code)]
    pub(crate) fn load_with_previous(
        home: &Path,
        live_roster: Option<HashMap<String, InstanceRef>>,
        previous: Option<&Self>,
    ) -> Self {
        Self::load_path(&crate::fleet::fleet_yaml_path(home), live_roster, previous)
    }

    pub(crate) fn load_path(
        path: &Path,
        live_roster: Option<HashMap<String, InstanceRef>>,
        previous: Option<&Self>,
    ) -> Self {
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::from_fleet(FleetConfig::default(), live_roster);
            }
            Err(error) => {
                tracing::warn!(error = %error, "team view fleet config unavailable");
                return previous.map(Self::stale).unwrap_or_else(Self::empty);
            }
        }
        let config = match FleetConfig::load(path) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(error = %error, "team view fleet config unavailable");
                return previous.map(Self::stale).unwrap_or_else(Self::empty);
            }
        };
        let mut view = Self::from_fleet(config, live_roster);
        if view.status == TeamViewStatus::Unavailable {
            if let Some(previous) = previous {
                return Self::stale(previous);
            }
        }
        view.last_roster = view.current_roster.clone();
        view
    }

    fn stale(previous: &Self) -> Self {
        let mut view = previous.clone();
        view.status = TeamViewStatus::Stale;
        view.current_roster.clear();
        view.roster_live = false;
        view
    }

    pub(crate) fn invalidate(&mut self) {
        self.status = if self.teams.is_empty() {
            TeamViewStatus::Unavailable
        } else {
            TeamViewStatus::Stale
        };
        self.current_roster.clear();
        self.roster_live = false;
    }

    pub(crate) fn badge(&self, member: &str, pane_ref: Option<&InstanceRef>) -> LeadBadge {
        let Some(team_name) = self.member_team.get(member) else {
            return LeadBadge::None;
        };
        let Some(team) = self.teams.get(team_name) else {
            return LeadBadge::None;
        };
        if team.orchestrator != member {
            return LeadBadge::None;
        }
        let expected = self.current_roster.get(member);
        match self.status {
            TeamViewStatus::Fresh => {
                if let (Some(expected), Some(pane_ref)) = (expected, pane_ref) {
                    if expected == pane_ref {
                        LeadBadge::Lead
                    } else {
                        LeadBadge::None
                    }
                } else if pane_ref.is_some() {
                    // A named replacement without the current identity must
                    // never inherit the old lead claim.
                    LeadBadge::None
                } else {
                    LeadBadge::Uncertain
                }
            }
            TeamViewStatus::Stale | TeamViewStatus::Unavailable => {
                if pane_ref.is_none() || self.last_roster.get(member) == pane_ref {
                    LeadBadge::Uncertain
                } else {
                    LeadBadge::None
                }
            }
        }
    }

    pub(crate) fn team_for_member(&self, member: &str) -> Option<&str> {
        self.member_team.get(member).map(String::as_str)
    }

    pub(crate) fn orchestrator_for_team(&self, team: &str) -> Option<&str> {
        self.teams
            .get(team)
            .map(|record| record.orchestrator.as_str())
    }

    pub(crate) fn authoritative_lead_for_members<'a, I>(&self, members: I) -> Option<&str>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let team = self.authoritative_tab_name(members)?;
        self.orchestrator_for_team(team)
    }

    pub(crate) fn roster_summary(&self) -> (usize, bool) {
        (self.current_roster.len(), self.roster_live)
    }

    pub(crate) fn roster_summary_for_team(&self, team: &str) -> (usize, bool) {
        let count = self
            .current_roster
            .keys()
            .filter(|member| {
                self.member_team
                    .get(*member)
                    .is_some_and(|name| name == team)
            })
            .count();
        (count, self.roster_live)
    }

    /// Return a team label only when every pane belongs to the same
    /// authoritative team. `Tab::name` is intentionally not consulted.
    pub(crate) fn authoritative_tab_name<'a, I>(&self, members: I) -> Option<&str>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut names = members.into_iter();
        let first = self.team_for_member(names.next()?)?;
        if names.all(|member| self.team_for_member(member) == Some(first)) {
            Some(first)
        } else {
            None
        }
    }

    pub(crate) fn status_label(&self) -> &'static str {
        match self.status {
            TeamViewStatus::Fresh => "Fresh",
            TeamViewStatus::Stale => "Stale",
            TeamViewStatus::Unavailable => "Unavailable",
        }
    }
}

fn validate_teams(
    configs: &HashMap<String, TeamConfig>,
) -> Option<(HashMap<String, TeamRecord>, HashMap<String, String>)> {
    let mut teams = HashMap::new();
    let mut member_team = HashMap::new();
    for (name, config) in configs {
        let orchestrator = config.orchestrator.clone()?;
        if config.members.is_empty() || !config.members.iter().any(|member| member == &orchestrator)
        {
            return None;
        }
        let mut members = config.members.clone();
        members.sort();
        members.dedup();
        if members.len() != config.members.len() {
            return None;
        }
        for member in &members {
            if member_team.insert(member.clone(), name.clone()).is_some() {
                return None;
            }
        }
        teams.insert(name.clone(), TeamRecord { orchestrator });
    }
    Some((teams, member_team))
}

pub(crate) fn clip_badge(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for ch in text.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + char_width > width {
            break;
        }
        out.push(ch);
        used += char_width;
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::fleet::{FleetConfig, TeamConfig};
    use crate::types::{InstanceId, InstanceRef};
    use std::collections::HashMap;
    use std::path::Path;

    fn id_ref(generation: u64) -> InstanceRef {
        InstanceRef::new(InstanceId::new(), generation)
    }

    fn fleet() -> FleetConfig {
        let mut config = FleetConfig::default();
        config.teams.insert(
            "ops".into(),
            TeamConfig {
                members: vec!["lead".into(), "member".into()],
                orchestrator: Some("lead".into()),
                description: None,
                created_at: None,
                source_repo: None,
                project_id: None,
                accept_from: Vec::new(),
            },
        );
        config
    }

    #[test]
    fn valid_online_orchestrator_is_the_only_lead() {
        let lead_ref = id_ref(1);
        let mut roster = HashMap::new();
        roster.insert("lead".into(), lead_ref);
        roster.insert("member".into(), id_ref(1));
        let member_ref = roster.get("member").copied();
        let view = TeamView::from_fleet(fleet(), Some(roster));
        assert_eq!(view.status(), TeamViewStatus::Fresh);
        assert_eq!(view.badge("lead", Some(&lead_ref)), LeadBadge::Lead);
        assert_eq!(view.badge("member", member_ref.as_ref()), LeadBadge::None);
    }

    #[test]
    fn same_name_different_incarnation_never_inherits_lead() {
        let lead_ref = id_ref(1);
        let replacement_ref = InstanceRef::new(lead_ref.instance_id, 2);
        let mut roster = HashMap::new();
        roster.insert("lead".into(), replacement_ref);
        let view = TeamView::from_fleet(fleet(), Some(roster));
        assert_eq!(view.badge("lead", Some(&lead_ref)), LeadBadge::None);
    }

    #[test]
    fn invalid_membership_is_unavailable_and_duplicate_ownership_fails_closed() {
        let mut config = fleet();
        config.teams.get_mut("ops").unwrap().orchestrator = Some("ghost".into());
        let view = TeamView::from_fleet(config, Some(HashMap::new()));
        assert_eq!(view.status(), TeamViewStatus::Unavailable);

        let mut config = fleet();
        config.teams.insert(
            "other".into(),
            TeamConfig {
                members: vec!["lead".into()],
                orchestrator: Some("lead".into()),
                description: None,
                created_at: None,
                source_repo: None,
                project_id: None,
                accept_from: Vec::new(),
            },
        );
        let view = TeamView::from_fleet(config, Some(HashMap::new()));
        assert_eq!(view.status(), TeamViewStatus::Unavailable);
    }

    #[test]
    fn strict_read_distinguishes_missing_malformed_and_stale() {
        let home = std::env::temp_dir().join(format!(
            "team-view-strict-read-{}",
            crate::types::InstanceId::new()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let missing = TeamView::load(Path::new(&home), None);
        assert_eq!(missing.status(), TeamViewStatus::Fresh);

        std::fs::write(home.join("fleet.yaml"), "teams: [").unwrap();
        let unavailable = TeamView::load(&home, None);
        assert_eq!(unavailable.status(), TeamViewStatus::Unavailable);

        std::fs::write(
            home.join("fleet.yaml"),
            "teams:\n  ops:\n    members: [lead]\n    orchestrator: lead\n",
        )
        .unwrap();
        let fresh = TeamView::load(&home, Some(HashMap::new()));
        assert_eq!(fresh.status(), TeamViewStatus::Fresh);

        std::fs::write(home.join("fleet.yaml"), "teams: [").unwrap();
        let stale = TeamView::load_with_previous(&home, None, Some(&fresh));
        assert_eq!(stale.status(), TeamViewStatus::Stale);
        assert_eq!(stale.badge("lead", None), LeadBadge::Uncertain);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn successful_reload_tracks_latest_roster_for_future_stale_claims() {
        let home = std::env::temp_dir().join(format!(
            "team-view-latest-roster-{}",
            crate::types::InstanceId::new()
        ));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("fleet.yaml"),
            "teams:\n  ops:\n    members: [lead]\n    orchestrator: lead\n",
        )
        .unwrap();

        let first_ref = id_ref(1);
        let mut first_roster = HashMap::new();
        first_roster.insert("lead".to_string(), first_ref);
        let first = TeamView::load(&home, Some(first_roster));

        let latest_ref = id_ref(2);
        let mut latest_roster = HashMap::new();
        latest_roster.insert("lead".to_string(), latest_ref);
        crate::fleet::invalidate_cache();
        let latest = TeamView::load_with_previous(&home, Some(latest_roster), Some(&first));
        std::fs::write(home.join("fleet.yaml"), "teams: [").unwrap();
        crate::fleet::invalidate_cache();
        let stale = TeamView::load_with_previous(&home, None, Some(&latest));

        assert_eq!(stale.status(), TeamViewStatus::Stale);
        assert_eq!(
            stale.badge("lead", Some(&latest_ref)),
            LeadBadge::Uncertain,
            "the latest live identity remains possible after degradation"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn broken_existing_symlink_is_not_treated_as_missing_fleet() {
        use std::os::unix::fs::symlink;

        let home = std::env::temp_dir().join(format!(
            "team-view-broken-link-{}",
            crate::types::InstanceId::new()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join("fleet.yaml");
        std::fs::write(
            &path,
            "teams:\n  ops:\n    members: [lead]\n    orchestrator: lead\n",
        )
        .unwrap();
        let lead_ref = id_ref(1);
        let mut roster = HashMap::new();
        roster.insert("lead".to_string(), lead_ref);
        let previous = TeamView::load(&home, Some(roster));

        std::fs::remove_file(&path).unwrap();
        symlink("missing-fleet.yaml", &path).unwrap();
        crate::fleet::invalidate_cache();
        let view = TeamView::load_with_previous(&home, None, Some(&previous));

        assert_eq!(view.status(), TeamViewStatus::Stale);
        assert_eq!(view.badge("lead", None), LeadBadge::Uncertain);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn narrow_badge_width_keeps_deterministic_prefix() {
        assert_eq!(clip_badge("[LEAD]", 0), "");
        assert_eq!(clip_badge("[LEAD]", 3), "[LE");
        assert_eq!(clip_badge("[LEAD?]", 5), "[LEAD");
    }
}
