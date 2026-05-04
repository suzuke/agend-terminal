//! Fleet view, monitor view, and uptime formatting.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Pure function: build fleet view text lines for testing.
pub fn build_fleet_view_lines(
    tasks: &[crate::tasks::Task],
    teams: &[crate::teams::Team],
    all_instances: &[String],
) -> Vec<String> {
    let mut agent_tasks: std::collections::HashMap<&str, Vec<&crate::tasks::Task>> =
        std::collections::HashMap::new();
    for t in tasks {
        if t.status == "claimed" {
            if let Some(ref a) = t.assignee {
                agent_tasks.entry(a.as_str()).or_default().push(t);
            }
        }
    }
    let mut lines = Vec::new();
    let mut assigned: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for team in teams {
        lines.push(format!(
            "═══ {} (orchestrator: {}) ═══",
            team.name,
            team.orchestrator.as_deref().unwrap_or("none")
        ));
        for member in &team.members {
            assigned.insert(member.as_str());
            let symbol = if agent_tasks.contains_key(member.as_str()) {
                "🟠"
            } else {
                "🟢"
            };
            let task_info = agent_tasks
                .get(member.as_str())
                .and_then(|ts| ts.first())
                .map(|t| format!(" → {} ({})", t.title, t.id))
                .unwrap_or_else(|| " idle".to_string());
            lines.push(format!("  {symbol} {member}{task_info}"));
        }
    }
    let mut unassigned: Vec<&str> = all_instances
        .iter()
        .filter(|n| !assigned.contains(n.as_str()))
        .map(String::as_str)
        .collect();
    unassigned.sort_unstable();
    if !unassigned.is_empty() {
        lines.push("═══ unassigned ═══".to_string());
        for name in &unassigned {
            let symbol = if agent_tasks.contains_key(name) {
                "🟠"
            } else {
                "🟢"
            };
            let task_info = agent_tasks
                .get(name)
                .and_then(|ts| ts.first())
                .map(|t| format!(" → {} ({})", t.title, t.id))
                .unwrap_or_else(|| " idle".to_string());
            lines.push(format!("  {symbol} {name}{task_info}"));
        }
    }
    lines
}

pub(super) fn render_monitor_view(frame: &mut Frame, area: Rect) {
    let metrics = crate::instance_monitor::latest_metrics();
    if metrics.is_empty() {
        frame.render_widget(
            Paragraph::new("  No instance metrics yet (waiting for first collection tick).")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return;
    }

    let header = Line::from(vec![Span::styled(
        format!(
            " {:<16} {:<12} {:<12} {:>8} {:>6} {:>10} {:>8} {:>7}",
            "NAME", "STATE", "HEALTH", "MEM(MB)", "CPU%", "UPTIME", "HB-LAG", "PICKUP"
        ),
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(Color::Cyan),
    )]);

    let mut lines = vec![header];
    for m in &metrics {
        let mem_str = m
            .rss_bytes
            .map(|b| format!("{:.1}", b as f64 / 1_048_576.0))
            .unwrap_or_else(|| "—".into());
        let cpu_str = m
            .cpu_percent
            .map(|c| format!("{c:.1}"))
            .unwrap_or_else(|| "—".into());
        let uptime_str = m
            .uptime_secs
            .map(format_uptime)
            .unwrap_or_else(|| "—".into());
        let hb_str = m
            .heartbeat_lag_secs
            .map(|s| format!("{s}s"))
            .unwrap_or_else(|| "—".into());

        let health_color = match m.health_state.as_str() {
            "ok" => Color::Green,
            "hung" | "crashed" | "unstable" => Color::Red,
            "rate_limit" => Color::Yellow,
            _ => Color::White,
        };

        lines.push(Line::from(vec![
            Span::raw(format!(" {:<16} {:<12} ", m.name, m.agent_state)),
            Span::styled(
                format!("{:<12}", m.health_state),
                Style::default().fg(health_color),
            ),
            Span::raw(format!(
                "{:>8} {:>6} {:>10} {:>8} {:>7}",
                mem_str, cpu_str, uptime_str, hb_str, m.pending_pickup_count
            )),
        ]));
    }

    let para = Paragraph::new(lines);
    frame.render_widget(para, area);
}

pub(super) fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

pub(super) fn render_fleet_view(
    frame: &mut Frame,
    tasks: &[crate::tasks::Task],
    area: Rect,
    home: &std::path::Path,
) {
    let teams = crate::teams::list_all(home);
    let all_instances: Vec<String> = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
        .ok()
        .map(|c| c.instance_names())
        .unwrap_or_default();
    let text_lines = build_fleet_view_lines(tasks, &teams, &all_instances);

    let lines: Vec<Line> = if text_lines.is_empty() {
        vec![Line::from(Span::styled(
            "No agents configured. Add instances to fleet.yaml.",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        text_lines
            .iter()
            .map(|l| {
                let style = if l.starts_with('═') {
                    if l.contains("unassigned") {
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    }
                } else {
                    Style::default().fg(Color::White)
                };
                Line::from(Span::styled(l.as_str(), style))
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_fleet_view_content() {
        let teams = vec![crate::teams::Team {
            name: "dev".to_string(),
            members: vec!["dev-lead".to_string(), "dev-impl".to_string()],
            orchestrator: Some("dev-lead".to_string()),
            description: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }];
        let tasks = vec![crate::tasks::Task {
            id: "t-1".to_string(),
            title: "busy work".to_string(),
            description: String::new(),
            status: "claimed".to_string(),
            priority: "normal".to_string(),
            assignee: Some("dev-impl".to_string()),
            routed_to: None,
            created_by: "lead".to_string(),
            depends_on: vec![],
            result: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            due_at: None,
            branch: None,
        }];
        let all_instances = vec![
            "dev-lead".to_string(),
            "dev-impl".to_string(),
            "general".to_string(),
        ];
        let lines = build_fleet_view_lines(&tasks, &teams, &all_instances);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("dev") && l.contains("orchestrator: dev-lead")),
            "must have team header: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("🟠") && l.contains("dev-impl") && l.contains("busy work")),
            "busy member must show task: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("🟢") && l.contains("dev-lead") && l.contains("idle")),
            "idle member must show idle: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("unassigned")),
            "must have unassigned group: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("general")),
            "unassigned agent must appear: {lines:?}"
        );
    }
}
