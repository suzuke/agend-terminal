//! Panel overlays: decisions, task board.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::overlay::render_overlay_frame;
use super::panels_fleet::{render_fleet_view, render_monitor_view};

pub fn render_decisions(frame: &mut Frame, items: &[crate::decisions::Decision], scroll: usize) {
    let count = items.len();
    let title = format!(" Decisions ({count}) | j/k scroll | q close ");
    let inner = render_overlay_frame(frame, Color::Yellow, &title);

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("  No decisions yet.").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    for (i, d) in items.iter().enumerate() {
        let marker = if i == scroll { "> " } else { "  " };
        let style = if i == scroll {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(
            format!("{marker}[{}] {}", d.scope, d.title),
            style,
        )));
        if i == scroll {
            lines.push(Line::from(Span::styled(
                format!(
                    "    by {} | {}",
                    d.author,
                    &d.created_at.get(..10).unwrap_or(&d.created_at)
                ),
                Style::default().fg(Color::DarkGray),
            )));
            for line in d.content.lines() {
                lines.push(Line::from(Span::styled(
                    format!("    {line}"),
                    Style::default().fg(Color::Gray),
                )));
            }
            if !d.tags.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("    tags: {}", d.tags.join(", ")),
                    Style::default().fg(Color::Cyan),
                )));
            }
            lines.push(Line::from(""));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_tasks(
    frame: &mut Frame,
    items: &[crate::tasks::Task],
    sel_col: usize,
    sel_row: usize,
    mode: &crate::app::TaskBoardMode,
    view: crate::app::BoardView,
    home: &std::path::Path,
) {
    use crate::app::{BoardView, TaskBoardMode};
    let count = items.len();
    let view_tabs = match view {
        BoardView::Tasks => "[t] tasks  [f] fleet  [s] status  [m] monitor",
        BoardView::Fleet => " [t] tasks [f] fleet  [s] status  [m] monitor",
        BoardView::Status => " [t] tasks  [f] fleet [s] status  [m] monitor",
        BoardView::Monitor => " [t] tasks  [f] fleet  [s] status [m] monitor",
    };
    let title = format!(" Board ({count}) | {view_tabs} | Tab switch | q close ");
    let inner = render_overlay_frame(frame, Color::Blue, &title);

    if matches!(view, BoardView::Monitor) {
        render_monitor_view(frame, inner);
        return;
    }

    if matches!(view, BoardView::Status) {
        let summary = crate::status_summary::build_summary(home);
        let lines: Vec<ratatui::text::Line> = summary
            .lines()
            .map(|l| ratatui::text::Line::from(l.to_string()))
            .collect();
        let para =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        frame.render_widget(para, inner);
        return;
    }

    if matches!(view, BoardView::Fleet) {
        render_fleet_view(frame, items, inner, home);
        return;
    }

    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("  No tasks yet.").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    let columns = task_board_columns(items);

    if matches!(mode, TaskBoardMode::Detail) {
        if let Some(task) = columns[sel_col].get(sel_row) {
            let mut lines = vec![
                Line::from(Span::styled(
                    &task.title,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!(
                        "Status: {}  Priority: {}  Assignee: {}",
                        task.status,
                        task.priority,
                        task.assignee.as_deref().unwrap_or("-")
                    ),
                    Style::default().fg(Color::Gray),
                )),
                Line::from(""),
            ];
            if !task.description.is_empty() {
                lines.push(Line::from(Span::styled(
                    &task.description,
                    Style::default().fg(Color::White),
                )));
                lines.push(Line::from(""));
            }
            if !task.depends_on.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("Depends on: {}", task.depends_on.join(", ")),
                    Style::default().fg(Color::Yellow),
                )));
            }
            if let Some(ref result) = task.result {
                lines.push(Line::from(Span::styled(
                    format!("Result: {result}"),
                    Style::default().fg(Color::Green),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Esc to go back",
                Style::default().fg(Color::DarkGray),
            )));
            frame.render_widget(Paragraph::new(lines), inner);
            return;
        }
    }

    let col_areas = ratatui::layout::Layout::horizontal([
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
    ])
    .split(inner);

    let col_titles = ["Backlog", "Open", "In Progress", "Done"];
    let done_visible = columns.get(3).map(|c| c.len()).unwrap_or(0);
    let col_title_strs: Vec<String> = col_titles
        .iter()
        .enumerate()
        .map(|(i, t)| {
            if i == 3 {
                format!(" {} ({}/14d) ", t, done_visible)
            } else {
                format!(" {} ({}) ", t, columns.get(i).map(|c| c.len()).unwrap_or(0))
            }
        })
        .collect();
    let col_colors = [Color::Gray, Color::Green, Color::Yellow, Color::DarkGray];

    for (ci, (tasks, area)) in columns.iter().zip(col_areas.iter()).enumerate() {
        let is_active = ci == sel_col;
        let border_color = if is_active {
            Color::Blue
        } else {
            col_colors[ci]
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Span::styled(
                &col_title_strs[ci],
                Style::default()
                    .fg(if is_active {
                        Color::Blue
                    } else {
                        col_colors[ci]
                    })
                    .add_modifier(Modifier::BOLD),
            ));
        let block_inner = block.inner(*area);
        frame.render_widget(block, *area);

        let mut lines: Vec<Line> = Vec::new();
        for (ri, t) in tasks.iter().enumerate() {
            let is_selected = is_active && ri == sel_row;
            let pri_badge = match t.priority.as_str() {
                "urgent" => "🔴",
                "high" => "🟠",
                "normal" => "🔵",
                _ => "⚪",
            };
            let blocked = if t.status == "blocked" { " 🔴" } else { "" };
            let assignee = t
                .assignee
                .as_deref()
                .map(|a| format!(" @{a}"))
                .unwrap_or_default();
            let text = format!("{pri_badge} {}{blocked}{assignee}", t.title);
            let style = if is_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
        frame.render_widget(Paragraph::new(lines), block_inner);
    }

    if inner.height > 2 && inner.width > 10 {
        let active_agents: std::collections::HashSet<&str> = columns[2]
            .iter()
            .filter_map(|t| t.assignee.as_deref())
            .collect();
        let status_text = if active_agents.is_empty() {
            "all idle".to_string()
        } else {
            let names: Vec<&str> = active_agents.into_iter().collect();
            format!("active: {}", names.join(", "))
        };
        let status_y = inner.y + inner.height.saturating_sub(1);
        let status_area = Rect::new(
            inner.x + 1,
            status_y,
            inner.width.saturating_sub(2).min(status_text.len() as u16),
            1,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(status_text, Style::default().fg(Color::Cyan))),
            status_area,
        );
    }

    if !matches!(mode, TaskBoardMode::Help) && inner.height > 1 && inner.width > 6 {
        let hint = "? help";
        let hint_x = inner.x + inner.width.saturating_sub(hint.len() as u16 + 1);
        let hint_y = inner.y + inner.height.saturating_sub(1);
        let hint_area = Rect::new(hint_x, hint_y, hint.len() as u16, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            hint_area,
        );
    }

    match mode {
        TaskBoardMode::NewTask { input } => {
            let w = 50u16.min(inner.width.saturating_sub(4));
            let popup = Rect::new(
                inner.x + (inner.width.saturating_sub(w)) / 2,
                inner.y + inner.height / 3,
                w,
                3,
            );
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green))
                .title(Span::styled(
                    " New Task ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
            let inner_popup = block.inner(popup);
            frame.render_widget(block, popup);
            let cursor = format!("{input}▏");
            frame.render_widget(
                Paragraph::new(cursor).style(Style::default().fg(Color::White)),
                inner_popup,
            );
        }
        TaskBoardMode::Assign {
            choices, selected, ..
        } => {
            let h = (choices.len() as u16 + 2).min(inner.height.saturating_sub(2));
            let w = 40u16.min(inner.width.saturating_sub(4));
            let popup = Rect::new(
                inner.x + (inner.width.saturating_sub(w)) / 2,
                inner.y + (inner.height.saturating_sub(h)) / 2,
                w,
                h,
            );
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(Span::styled(
                    " Assign to ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
            let inner_popup = block.inner(popup);
            frame.render_widget(block, popup);
            let mut lines: Vec<Line> = Vec::new();
            for (i, (display, _value)) in choices.iter().enumerate() {
                let style = if i == *selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                lines.push(Line::from(Span::styled(display.as_str(), style)));
            }
            frame.render_widget(Paragraph::new(lines), inner_popup);
        }
        TaskBoardMode::Help => {
            let text = vec![
                Line::from(Span::styled(
                    "Task Board Help",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from("───────────────"),
                Line::from("  n           New task"),
                Line::from("  ↑↓ / j k    Select task in column"),
                Line::from("  ←→ / h l    Switch column"),
                Line::from("  H / L       Move task left/right (change status)"),
                Line::from("  Shift+D     Mark done from any column"),
                Line::from("  a           Assign to agent or team"),
                Line::from("  d           Cancel task"),
                Line::from("  Enter       View task detail"),
                Line::from("  Esc / q     Close Task Board"),
                Line::from(""),
                Line::from(Span::styled(
                    "Press ? or Esc to close help",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            let h = (text.len() as u16 + 2).min(inner.height.saturating_sub(2));
            let w = 52u16.min(inner.width.saturating_sub(4));
            let popup = Rect::new(
                inner.x + (inner.width.saturating_sub(w)) / 2,
                inner.y + (inner.height.saturating_sub(h)) / 2,
                w,
                h,
            );
            frame.render_widget(Clear, popup);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(
                    " ? Help ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            let inner_popup = block.inner(popup);
            frame.render_widget(block, popup);
            frame.render_widget(Paragraph::new(text), inner_popup);
        }
        _ => {}
    }
}

/// Group tasks into 4 kanban columns: Backlog, Open, In Progress, Done.
/// Sorted by priority desc then created_at asc within each column.
/// Cancelled tasks are excluded.
pub fn task_board_columns(items: &[crate::tasks::Task]) -> [Vec<&crate::tasks::Task>; 4] {
    let mut backlog: Vec<&crate::tasks::Task> = Vec::new();
    let mut open: Vec<&crate::tasks::Task> = Vec::new();
    let mut in_progress: Vec<&crate::tasks::Task> = Vec::new();
    let mut done: Vec<&crate::tasks::Task> = Vec::new();

    for t in items {
        match t.status.as_str() {
            "cancelled" => {}
            "open" | "blocked" if t.priority == "low" => backlog.push(t),
            "open" | "blocked" => open.push(t),
            "claimed" => in_progress.push(t),
            "done" => done.push(t),
            _ => open.push(t),
        }
    }

    fn sort_col(col: &mut Vec<&crate::tasks::Task>) {
        col.sort_by(|a, b| {
            let pri_ord = |p: &str| -> u8 {
                match p {
                    "urgent" => 0,
                    "high" => 1,
                    "normal" => 2,
                    "low" => 3,
                    _ => 4,
                }
            };
            pri_ord(&a.priority)
                .cmp(&pri_ord(&b.priority))
                .then(a.created_at.cmp(&b.created_at))
        });
    }

    sort_col(&mut backlog);
    sort_col(&mut open);
    sort_col(&mut in_progress);
    in_progress.sort_by(|a, b| {
        a.assignee
            .as_deref()
            .unwrap_or("")
            .cmp(b.assignee.as_deref().unwrap_or(""))
    });
    sort_col(&mut done);

    [backlog, open, in_progress, done]
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_task(
        title: &str,
        status: &str,
        priority: &str,
        created_at: &str,
    ) -> crate::tasks::Task {
        crate::tasks::Task {
            id: title.to_string(),
            title: title.to_string(),
            description: String::new(),
            status: status.to_string(),
            priority: priority.to_string(),
            assignee: None,
            routed_to: None,
            created_by: String::new(),
            depends_on: Vec::new(),
            result: None,
            created_at: created_at.to_string(),
            updated_at: String::new(),
            due_at: None,
            branch: None,
        }
    }

    #[test]
    fn task_board_groups_by_status() {
        let tasks = vec![
            make_task("backlog-item", "open", "low", "2026-01-01"),
            make_task("open-item", "open", "high", "2026-01-02"),
            make_task("wip", "claimed", "normal", "2026-01-03"),
            make_task("finished", "done", "normal", "2026-01-04"),
            make_task("cancelled-item", "cancelled", "normal", "2026-01-05"),
            make_task("blocked-low", "blocked", "low", "2026-01-06"),
        ];
        let [backlog, open, in_progress, done] = task_board_columns(&tasks);
        assert_eq!(backlog.len(), 2, "open+low and blocked+low → backlog");
        assert_eq!(open.len(), 1, "open+high → open");
        assert_eq!(in_progress.len(), 1, "claimed → in progress");
        assert_eq!(done.len(), 1, "done → done");
    }

    #[test]
    fn task_board_sorts_by_priority_then_created() {
        let tasks = vec![
            make_task("normal-old", "open", "normal", "2026-01-01"),
            make_task("urgent-new", "open", "urgent", "2026-01-03"),
            make_task("normal-new", "open", "normal", "2026-01-02"),
            make_task("high-old", "open", "high", "2026-01-01"),
        ];
        let [_, open, _, _] = task_board_columns(&tasks);
        assert_eq!(open[0].title, "urgent-new", "urgent first");
        assert_eq!(open[1].title, "high-old", "high second");
        assert_eq!(open[2].title, "normal-old", "normal oldest third");
        assert_eq!(open[3].title, "normal-new", "normal newest last");
    }

    fn render_task_board_to_string(mode: &crate::app::TaskBoardMode) -> String {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
        let tasks = vec![crate::tasks::Task {
            id: "t-1".into(),
            title: "test task".into(),
            description: String::new(),
            status: "open".into(),
            priority: "normal".into(),
            assignee: None,
            routed_to: None,
            depends_on: Vec::new(),
            result: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            created_by: "test".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            due_at: None,
            branch: None,
        }];
        terminal
            .draw(|frame| {
                render_tasks(
                    frame,
                    &tasks,
                    0,
                    0,
                    mode,
                    crate::app::BoardView::Tasks,
                    std::path::Path::new("/tmp"),
                )
            })
            .expect("test terminal draw should succeed");
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn task_board_footer_shows_help_hint() {
        let output = render_task_board_to_string(&crate::app::TaskBoardMode::Board);
        assert!(
            output.contains("? help"),
            "Board mode should show '? help' hint, got:\n{output}"
        );
    }

    #[test]
    fn task_board_help_mode_hides_footer() {
        let output = render_task_board_to_string(&crate::app::TaskBoardMode::Help);
        assert!(
            !output.contains("? help"),
            "Help mode should NOT show '? help' hint, got:\n{output}"
        );
    }
}
