//! Canonical team ordering and deterministic membership diagnostics.

use crate::fleet::FleetConfig;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TeamDiagnostic {
    pub(crate) team: String,
    pub(crate) member: Option<String>,
    pub(crate) code: &'static str,
}

impl TeamDiagnostic {
    pub(crate) fn label(&self) -> String {
        let member = self.member.as_deref().unwrap_or("-");
        format!("team={} member={} code={}", self.team, member, self.code)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TeamOrderGroup {
    pub(crate) name: String,
    pub(crate) members: Vec<String>,
    pub(crate) stale_members: Vec<String>,
    pub(crate) corrupt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TeamOrderPlan {
    pub(crate) groups: Vec<TeamOrderGroup>,
    pub(crate) ungrouped: Vec<String>,
    pub(crate) diagnostics: Vec<TeamDiagnostic>,
}

impl TeamOrderPlan {
    pub(crate) fn diagnostics_label(&self) -> Option<String> {
        (!self.diagnostics.is_empty()).then(|| {
            self.diagnostics
                .iter()
                .map(TeamDiagnostic::label)
                .collect::<Vec<_>>()
                .join("; ")
        })
    }
}

pub(crate) fn plan_team_order<'a, I>(fleet: &FleetConfig, live_names: I) -> TeamOrderPlan
where
    I: IntoIterator<Item = &'a str>,
{
    let live: HashSet<&str> = live_names.into_iter().collect();
    let mut diagnostics = Vec::new();
    let mut ownership: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut corrupt = HashSet::new();

    let mut team_names: Vec<&str> = fleet.teams.keys().map(String::as_str).collect();
    team_names.sort_unstable();
    for team_name in &team_names {
        let config = &fleet.teams[*team_name];
        let mut seen = HashSet::new();
        for member in &config.members {
            if !seen.insert(member.as_str()) {
                diagnostics.push(TeamDiagnostic {
                    team: (*team_name).to_string(),
                    member: Some(member.clone()),
                    code: "duplicate-member",
                });
                corrupt.insert(*team_name);
            }
            let owners = ownership.entry(member).or_default();
            if !owners.contains(team_name) {
                owners.push(team_name);
            }
        }
        if let Some(orchestrator) = config.orchestrator.as_deref() {
            if !config.members.iter().any(|member| member == orchestrator) {
                diagnostics.push(TeamDiagnostic {
                    team: (*team_name).to_string(),
                    member: Some(orchestrator.to_string()),
                    code: "orchestrator-not-member",
                });
                corrupt.insert(*team_name);
            }
        } else {
            diagnostics.push(TeamDiagnostic {
                team: (*team_name).to_string(),
                member: None,
                code: "missing-orchestrator",
            });
        }
    }

    for (member, teams) in ownership {
        if teams.len() < 2 {
            continue;
        }
        let mut sorted_teams = teams;
        sorted_teams.sort_unstable();
        for team in sorted_teams {
            diagnostics.push(TeamDiagnostic {
                team: team.to_string(),
                member: Some(member.to_string()),
                code: "cross-team-member",
            });
            corrupt.insert(team);
        }
    }

    diagnostics.sort_by(|a, b| {
        a.team
            .cmp(&b.team)
            .then_with(|| a.member.cmp(&b.member))
            .then(a.code.cmp(b.code))
    });

    let mut groups = Vec::new();
    let mut grouped_members = HashSet::new();
    for team_name in team_names {
        let config = &fleet.teams[team_name];
        let stale_members: Vec<String> = config
            .members
            .iter()
            .filter(|member| !live.contains(member.as_str()))
            .cloned()
            .collect();
        let mut members = if corrupt.contains(team_name) {
            Vec::new()
        } else {
            let mut ordered = Vec::new();
            if let Some(orchestrator) = config.orchestrator.as_deref() {
                if live.contains(orchestrator) {
                    ordered.push(orchestrator.to_string());
                }
            }
            for member in &config.members {
                if live.contains(member.as_str())
                    && Some(member.as_str()) != config.orchestrator.as_deref()
                {
                    ordered.push(member.clone());
                }
            }
            ordered
        };
        members.dedup();
        if !corrupt.contains(team_name) {
            grouped_members.extend(members.iter().cloned());
        }
        groups.push(TeamOrderGroup {
            name: team_name.to_string(),
            members,
            stale_members,
            corrupt: corrupt.contains(team_name),
        });
    }

    let mut ungrouped: Vec<String> = live
        .into_iter()
        .filter(|member| !grouped_members.contains(*member))
        .map(str::to_string)
        .collect();
    ungrouped.sort_unstable();
    TeamOrderPlan {
        groups,
        ungrouped,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_plan_uses_configured_order_and_stable_diagnostics() {
        let fleet: FleetConfig = serde_yaml_ng::from_str(
            "teams:\n  zeta:\n    members: [lead, second, second]\n    orchestrator: ghost\n  alpha:\n    members: [shared]\n    orchestrator: shared\n",
        )
        .expect("valid team-order fixture");
        let plan = plan_team_order(&fleet, ["second", "lead", "shared", "extra"]);
        assert_eq!(plan.groups[0].name, "alpha");
        assert_eq!(plan.groups[1].name, "zeta");
        assert_eq!(plan.groups[0].members, vec!["shared"]);
        assert_eq!(plan.groups[1].members, Vec::<String>::new());
        assert_eq!(plan.ungrouped, vec!["extra", "lead", "second"]);
        assert_eq!(
            plan.diagnostics
                .iter()
                .map(TeamDiagnostic::label)
                .collect::<Vec<_>>(),
            vec![
                "team=zeta member=ghost code=orchestrator-not-member",
                "team=zeta member=second code=duplicate-member",
            ]
        );
    }

    #[test]
    fn absent_configured_members_are_stale_without_corrupting_order() {
        let fleet: FleetConfig = serde_yaml_ng::from_str(
            "teams:\n  svc:\n    members: [lead, absent, member]\n    orchestrator: lead\n",
        )
        .expect("valid team-order fixture");
        let plan = plan_team_order(&fleet, ["member", "lead"]);
        assert_eq!(plan.groups[0].members, vec!["lead", "member"]);
        assert_eq!(plan.groups[0].stale_members, vec!["absent"]);
        assert!(!plan.groups[0].corrupt);
        assert!(plan.diagnostics.is_empty());
    }

    #[test]
    fn cross_team_members_fail_closed_and_remain_sorted_unassigned() {
        let fleet: FleetConfig = serde_yaml_ng::from_str(
            "teams:\n  zeta:\n    members: [shared, z-lead]\n    orchestrator: z-lead\n  alpha:\n    members: [shared, a-lead]\n    orchestrator: a-lead\n",
        )
        .expect("valid team-order fixture");
        let plan = plan_team_order(&fleet, ["shared", "z-lead", "a-lead", "extra"]);
        assert!(plan
            .groups
            .iter()
            .all(|group| { group.corrupt && (group.name == "alpha" || group.name == "zeta") }));
        assert_eq!(plan.ungrouped, vec!["a-lead", "extra", "shared", "z-lead"]);
        assert_eq!(
            plan.diagnostics
                .iter()
                .map(TeamDiagnostic::label)
                .collect::<Vec<_>>(),
            vec![
                "team=alpha member=shared code=cross-team-member",
                "team=zeta member=shared code=cross-team-member",
            ]
        );
    }
}
