//! Fleet view, monitor view, and uptime formatting.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

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
    let mut all_instances: Vec<String> =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .map(|c| c.instance_names())
            .unwrap_or_default();
    // #1200: stable sort to prevent frame-to-frame jitter (HashMap iteration order).
    all_instances.sort_unstable();
    let metrics = crate::instance_monitor::latest_metrics();

    // Build a lookup of live metrics by instance name.
    let metrics_map: std::collections::HashMap<&str, &crate::instance_monitor::InstanceMetrics> =
        metrics.iter().map(|m| (m.name.as_str(), m)).collect();

    // Header
    let header = Line::from(Span::styled(
        format!(
            " {:<16} {:<12} {:<10} {:<30} {:<20}",
            "AGENT", "STATE", "HEALTH", "TASK", "BRANCH"
        ),
        Style::default()
            .add_modifier(Modifier::BOLD)
            .fg(Color::Cyan),
    ));

    let mut lines = vec![header];

    // Build agent→task mapping
    let mut agent_tasks: std::collections::HashMap<&str, &crate::tasks::Task> =
        std::collections::HashMap::new();
    for t in tasks {
        if matches!(
            t.status,
            crate::task_events::TaskStatus::Claimed | crate::task_events::TaskStatus::InProgress
        ) {
            if let Some(ref a) = t.assignee {
                agent_tasks.entry(a.as_str()).or_insert(t);
            }
        }
    }

    // #1200: sort teams + members for fully deterministic render order.
    let mut sorted_teams = teams.clone();
    sorted_teams.sort_by(|a, b| a.name.cmp(&b.name));

    let mut assigned: std::collections::HashSet<String> = std::collections::HashSet::new();
    for team in &sorted_teams {
        lines.push(Line::from(Span::styled(
            format!("═══ {} ═══", team.name),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        let mut members = team.members.clone();
        members.sort_unstable();
        for member in &members {
            assigned.insert(member.clone());
            lines.push(build_agent_line(member, &metrics_map, &agent_tasks, home));
        }
    }
    let mut unassigned: Vec<&str> = all_instances
        .iter()
        .filter(|n| !assigned.contains(n.as_str()))
        .map(String::as_str)
        .collect();
    unassigned.sort_unstable();
    if !unassigned.is_empty() {
        lines.push(Line::from(Span::styled(
            "═══ unassigned ═══",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )));
        for name in &unassigned {
            lines.push(build_agent_line(name, &metrics_map, &agent_tasks, home));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn build_agent_line<'a>(
    name: &str,
    metrics_map: &std::collections::HashMap<&str, &crate::instance_monitor::InstanceMetrics>,
    agent_tasks: &std::collections::HashMap<&str, &crate::tasks::Task>,
    home: &std::path::Path,
) -> Line<'a> {
    let (state, health, health_color) = metrics_map
        .get(name)
        .map(|m| {
            let color = match m.health_state.as_str() {
                "healthy" | "ok" => Color::Green,
                "hung" | "crashed" | "failed" | "error_loop" => Color::Red,
                "rate_limit" | "recovering" | "unstable" => Color::Yellow,
                "idle_long" => Color::DarkGray,
                "paused" => Color::Magenta,
                _ => Color::White,
            };
            (
                m.agent_state.as_str().to_string(),
                m.health_state.clone(),
                color,
            )
        })
        .unwrap_or_else(|| ("stopped".to_string(), "—".to_string(), Color::DarkGray));

    let task_str = agent_tasks
        .get(name)
        .map(|t| {
            let title: String = t.title.chars().take(28).collect();
            title
        })
        .unwrap_or_else(|| "—".to_string());

    let branch = crate::binding::read(home, name)
        .and_then(|v| v["branch"].as_str().map(String::from))
        .unwrap_or_else(|| "—".to_string());
    let branch_short: String = branch.chars().take(18).collect();

    let symbol = if state == "stopped" { "○" } else { "●" };
    Line::from(vec![
        Span::styled(
            format!(" {symbol} {:<15}", name),
            Style::default().fg(if state == "stopped" {
                Color::DarkGray
            } else {
                Color::White
            }),
        ),
        Span::styled(format!("{:<12}", state), Style::default().fg(health_color)),
        Span::styled(format!("{:<10}", health), Style::default().fg(health_color)),
        Span::styled(
            format!("{:<30}", task_str),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!("{:<20}", branch_short),
            Style::default().fg(Color::Blue),
        ),
    ])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-panels-fleet-{}-{}-{}",
            tag,
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Drive the REAL `render_fleet_view` end-to-end (teams + instances loaded
    /// from fleet.yaml, rendered into a TestBackend buffer). The earlier version
    /// tested a `#[cfg(test)]`-only `build_fleet_view_lines` re-implementation
    /// that DIVERGED from production (different symbols/header/idle text), so it
    /// could pass while the rendered output was wrong.
    #[test]
    fn render_fleet_view_groups_team_members_and_unassigned() {
        let home = tmp_home("render");
        // Instances live in fleet.yaml; `instance_names()` feeds the unassigned
        // group, and `teams::create` (the prod write path) records the team.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  dev-lead:\n    backend: claude\n  \
             dev-impl:\n    backend: claude\n  general:\n    backend: claude\n",
        )
        .unwrap();
        crate::teams::create(
            &home,
            &serde_json::json!({
                "name": "dev",
                "members": ["dev-lead", "dev-impl"],
                "orchestrator": "dev-lead",
            }),
        );

        let tasks = vec![crate::tasks::Task {
            id: "t-1".to_string(),
            title: "busy work".to_string(),
            description: String::new(),
            status: crate::task_events::TaskStatus::Claimed,
            priority: crate::task_events::TaskPriority::Normal,
            assignee: Some("dev-impl".to_string()),
            routed_to: None,
            created_by: "lead".to_string(),
            depends_on: vec![],
            result: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            due_at: None,
            branch: None,
            started_at: None,
            eta_secs: None,
            auto_release_on_verdict: None,
            tags: vec![],
            parent_id: None,
            metadata: std::collections::BTreeMap::new(),
        }];

        let backend = ratatui::backend::TestBackend::new(110, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_fleet_view(frame, &tasks, frame.area(), &home);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            out.push('\n');
        }

        assert!(out.contains("═══ dev ═══"), "team header missing:\n{out}");
        assert!(
            out.contains("dev-impl") && out.contains("busy work"),
            "claimed-task member must show its task title:\n{out}"
        );
        assert!(out.contains("dev-lead"), "team member missing:\n{out}");
        assert!(
            out.contains("unassigned") && out.contains("general"),
            "non-team instance must appear under unassigned:\n{out}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
