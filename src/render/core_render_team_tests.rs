#![allow(clippy::unwrap_used)]

use super::*;
use crate::layout::{Pane, PaneSource};
use crate::team_view::TeamView;
use crate::vterm::VTerm;

#[test]
fn real_pane_title_entry_point_renders_authoritative_lead_badge() {
    let lead_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 7);
    let config: crate::fleet::FleetConfig = serde_yaml_ng::from_str(
        "teams:\n  ops:\n    members: [lead, member]\n    orchestrator: lead\n",
    )
    .unwrap();
    let mut roster = HashMap::new();
    roster.insert("lead".to_string(), lead_ref);
    let view = TeamView::from_fleet(config, Some(roster));
    let mut pane = Pane {
        agent_name: "lead".into(),
        instance_id: lead_ref.instance_id,
        instance_ref: Some(lead_ref),
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: Some("lead".into()),
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    let segments = pane_title_segments_with_team(
        &pane,
        Style::default(),
        Some(AgentState::Idle),
        false,
        Some(&view),
    );
    assert!(
        segments.iter().any(|(text, _)| text == " [LEAD]"),
        "real title segmentation must include the authoritative badge: {segments:?}"
    );
    let (_, badge_style) = segments.iter().find(|(text, _)| text == " [LEAD]").unwrap();
    assert_eq!(badge_style.fg, Some(Color::LightGreen));
    assert!(badge_style.add_modifier.contains(Modifier::BOLD));
    pane.instance_ref = Some(crate::types::InstanceRef::new(lead_ref.instance_id, 8));
    let segments = pane_title_segments_with_team(
        &pane,
        Style::default(),
        Some(AgentState::Idle),
        false,
        Some(&view),
    );
    assert!(!segments.iter().any(|(text, _)| text.contains("[LEAD]")));
}

#[test]
fn focused_status_entry_point_renders_lead_and_roster_summary() {
    let lead_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 7);
    let config: crate::fleet::FleetConfig =
        serde_yaml_ng::from_str("teams:\n  ops:\n    members: [lead]\n    orchestrator: lead\n")
            .unwrap();
    let mut roster = HashMap::new();
    roster.insert("lead".to_string(), lead_ref);
    let view = TeamView::from_fleet(config, Some(roster));
    let pane = Pane {
        agent_name: "lead".into(),
        instance_id: lead_ref.instance_id,
        instance_ref: Some(lead_ref),
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: Some("lead".into()),
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    let mut layout = Layout::new();
    layout.add_tab(crate::layout::Tab::new("ops".into(), pane));
    let backend = ratatui::backend::TestBackend::new(120, 1);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            render_status_bar_with_team(
                frame,
                frame.area(),
                &layout,
                TelegramStatus::NotConfigured,
                false,
                0,
                crate::runtime::AgentListMode::Live,
                Some(&view),
            );
        })
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        text.contains("team:ops"),
        "status must identify the team: {text:?}"
    );
    assert!(
        text.contains("lead:lead"),
        "status must identify the lead: {text:?}"
    );
    assert!(
        text.contains("roster:1"),
        "status must summarize roster size: {text:?}"
    );
    assert!(
        text.contains("live:yes"),
        "status must expose live roster state: {text:?}"
    );
    assert!(
        text.contains("Fresh"),
        "status must expose freshness: {text:?}"
    );
}

#[test]
fn focused_status_entry_point_scopes_roster_summary_to_active_team_3629() {
    let lead_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 7);
    let member_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 3);
    let other_lead_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 4);
    let config: crate::fleet::FleetConfig = serde_yaml_ng::from_str(
        "teams:\n  ops:\n    members: [lead, member]\n    orchestrator: lead\n  infra:\n    members: [infra-lead]\n    orchestrator: infra-lead\n",
    )
    .unwrap();
    let mut roster = HashMap::new();
    roster.insert("lead".to_string(), lead_ref);
    roster.insert("member".to_string(), member_ref);
    roster.insert("infra-lead".to_string(), other_lead_ref);
    let view = TeamView::from_fleet(config, Some(roster));
    let pane = Pane {
        agent_name: "lead".into(),
        instance_id: lead_ref.instance_id,
        instance_ref: Some(lead_ref),
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        scroll_offset: 0,
        has_notification: false,
        pending_notification_count: 0,
        pending_decision_count: 0,
        fleet_instance_name: Some("lead".into()),
        last_input_at: None,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    let mut layout = Layout::new();
    layout.add_tab(crate::layout::Tab::new("ops".into(), pane));
    let backend = ratatui::backend::TestBackend::new(120, 1);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            render_status_bar_with_team(
                frame,
                frame.area(),
                &layout,
                TelegramStatus::NotConfigured,
                false,
                0,
                crate::runtime::AgentListMode::Live,
                Some(&view),
            );
        })
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        text.contains("roster:2"),
        "status must count only active-team roster members: {text:?}"
    );
    assert!(!text.contains("roster:3"));
}

#[test]
fn status_entry_point_surfaces_deterministic_team_diagnostics_3630() {
    let config: crate::fleet::FleetConfig =
        serde_yaml_ng::from_str("teams:\n  ops:\n    members: [lead]\n    orchestrator: ghost\n")
            .unwrap();
    let view = TeamView::from_fleet(config, Some(HashMap::new()));
    let mut layout = Layout::new();
    let pane = Pane {
        agent_name: "lead".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        scroll_offset: 0,
        has_notification: false,
        selection: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        fleet_instance_name: Some("lead".into()),
        last_input_at: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    layout.add_tab(crate::layout::Tab::new("ops".into(), pane));
    let backend = ratatui::backend::TestBackend::new(240, 1);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            render_status_bar_with_team(
                frame,
                frame.area(),
                &layout,
                TelegramStatus::NotConfigured,
                false,
                0,
                crate::runtime::AgentListMode::Live,
                Some(&view),
            );
        })
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        text.contains("team-config:team=ops member=ghost code=orchestrator-not-member"),
        "team diagnostics must be visible in the status bar: {text:?}"
    );
}
