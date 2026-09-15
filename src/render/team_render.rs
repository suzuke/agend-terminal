use crate::channel::TelegramStatus;
use crate::layout::Layout;
use crate::team_view::TeamView;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::{layout::Alignment, Frame};

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_status_bar_with_team(
    frame: &mut Frame,
    area: Rect,
    layout: &Layout,
    telegram: TelegramStatus,
    binary_stale: bool,
    pending_decisions: usize,
    daemon_list_mode: crate::runtime::AgentListMode,
    team_view: Option<&TeamView>,
) {
    let mut spans = Vec::new();

    if let Some(hint) = daemon_list_mode.hint() {
        spans.push(Span::styled(
            format!(" ! {hint} "),
            Style::default()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // #1027: operator-facing indicator for "running daemon's binary is
    // older than the on-disk binary; restart to pick up new code".
    // Replaces the previous inbox-emit path (which routed to agents
    // who cannot restart the daemon). Sticky-true until process
    // restart — see mcp_registry_watcher module-doc.
    if binary_stale {
        spans.push(Span::styled(
            " ! daemon binary stale (restart) ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let mut agent_count = 0;
    let mut total = 0;
    for tab in &layout.tabs {
        total += tab.root().pane_count();
        agent_count += tab.root().agent_count();
    }

    if agent_count > 0 {
        spans.push(Span::styled(
            format!(" {agent_count} agent(s) "),
            Style::default().fg(Color::Cyan),
        ));
    }
    if total > agent_count {
        spans.push(Span::styled(
            format!(" {total} pane(s) "),
            Style::default().fg(Color::White),
        ));
    }
    // #2313 P2b: passive discoverability badge for decision-board questions
    // awaiting an operator answer — no popup/sound, just a status-line count.
    // `pending_decisions` is the fleet-wide `decisions::count_pending(home)`
    // total (NOT summed from open panes — an author's pane may not be open in
    // this layout), refreshed by `sync_decision_badge_state` (app/mod.rs) on
    // the same ~1s throttle as the per-pane notification badge.
    if pending_decisions > 0 {
        spans.push(Span::styled(
            format!(" 🔴 {pending_decisions} decisions pending "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if let Some(tab) = layout.active_tab() {
        if let (Some(pane), Some(team_view)) = (tab.focused_pane(), team_view) {
            if let Some(team) = pane
                .fleet_instance_name
                .as_deref()
                .and_then(|name| team_view.team_for_member(name))
            {
                let lead = team_view.orchestrator_for_team(team).unwrap_or("unknown");
                let (roster_count, roster_live) = team_view.roster_summary_for_team(team);
                spans.push(Span::styled(
                    format!(
                        " team:{team} lead:{lead} roster:{roster_count} live:{} {} ",
                        if roster_live { "yes" } else { "no" },
                        team_view.status_label()
                    ),
                    Style::default().fg(Color::LightCyan),
                ));
            } else {
                let (roster_count, roster_live) = team_view.roster_summary();
                spans.push(Span::styled(
                    format!(
                        " team:none lead:none roster:{roster_count} live:{} {} ",
                        if roster_live { "yes" } else { "no" },
                        team_view.status_label()
                    ),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        if let Some(preset) = tab.last_layout {
            spans.push(Span::styled(
                format!(" [{}] ", preset.name()),
                Style::default().fg(Color::Yellow),
            ));
        }
    }

    match telegram {
        TelegramStatus::Connected => {
            spans.push(Span::styled(
                " TG ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        TelegramStatus::NoToken => {
            spans.push(Span::styled(
                " TG(no token) ",
                Style::default().fg(Color::Yellow),
            ));
        }
        TelegramStatus::NotConfigured => {}
    }

    // #1071: Clear pre-render (single Clear before BOTH bars). The two
    // Paragraphs render to the same area but only cover cells where their
    // own span text falls; cells in the middle gap between them — and any
    // trailing cells beyond shorter content compared to a prior frame —
    // would otherwise retain prior chars.
    frame.render_widget(Clear, area);
    let left_bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(left_bar, area);

    let right_hint = Line::from(vec![
        Span::styled(
            "Ctrl+B c new | : cmd | n/p switch | d detach | ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            "Ctrl+B ? help ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let right_bar = Paragraph::new(right_hint)
        .alignment(Alignment::Right)
        .style(Style::default().bg(Color::DarkGray));
    frame.render_widget(right_bar, area);
}
