//! Authoritative team view tests and implementation live here.

#[cfg(test)]
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
                ..Default::default()
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
        let view = TeamView::from_fleet(fleet(), Some(roster));
        assert_eq!(view.status(), TeamViewStatus::Fresh);
        assert_eq!(view.badge("lead", Some(lead_ref)), LeadBadge::Lead);
        assert_eq!(view.badge("member", roster.get("member")), LeadBadge::None);
    }

    #[test]
    fn same_name_different_incarnation_never_inherits_lead() {
        let lead_ref = id_ref(1);
        let replacement_ref = InstanceRef::new(lead_ref.instance_id, 2);
        let mut roster = HashMap::new();
        roster.insert("lead".into(), replacement_ref);
        let view = TeamView::from_fleet(fleet(), Some(roster));
        assert_eq!(view.badge("lead", Some(replacement_ref)), LeadBadge::None);
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
                ..Default::default()
            },
        );
        let view = TeamView::from_fleet(config, Some(HashMap::new()));
        assert_eq!(view.status(), TeamViewStatus::Unavailable);
    }

    #[test]
    fn strict_read_distinguishes_missing_malformed_and_stale() {
        let home = tempfile::tempdir().unwrap();
        let missing = TeamView::load(Path::new(home.path()), None);
        assert_eq!(missing.status(), TeamViewStatus::Fresh);

        std::fs::write(home.path().join("fleet.yaml"), "teams: [").unwrap();
        let unavailable = TeamView::load(home.path(), None);
        assert_eq!(unavailable.status(), TeamViewStatus::Unavailable);

        std::fs::write(
            home.path().join("fleet.yaml"),
            "teams:\n  ops:\n    members: [lead]\n    orchestrator: lead\n",
        )
        .unwrap();
        let fresh = TeamView::load(home.path(), Some(HashMap::new()));
        assert_eq!(fresh.status(), TeamViewStatus::Fresh);

        std::fs::write(home.path().join("fleet.yaml"), "teams: [").unwrap();
        let stale = TeamView::load_with_previous(home.path(), None, Some(&fresh));
        assert_eq!(stale.status(), TeamViewStatus::Stale);
        assert_eq!(stale.badge("lead", None), LeadBadge::Uncertain);
    }

    #[test]
    fn narrow_badge_width_keeps_deterministic_prefix() {
        assert_eq!(clip_badge("[LEAD]", 0), "");
        assert_eq!(clip_badge("[LEAD]", 3), "[LE");
        assert_eq!(clip_badge("[LEAD?]", 5), "[LEAD");
    }
}
