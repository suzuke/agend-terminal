//! Panel overlays: decisions, task board.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::collections::HashSet;

use super::overlay::render_overlay_frame;
use super::panels_fleet::{render_fleet_view, render_monitor_view};

/// #827: fetch the daemon's live runtime agent registry. Returns
/// `Some(set)` on success and `None` when `api::call(LIST)` fails.
///
/// The `None`-vs-`Some(empty)` distinction matters at the filter site:
/// `None` falls back to current (unfiltered) behavior so a degraded
/// daemon doesn't make the indicator misleadingly report "all idle".
/// Per-render call cost is one localhost TCP round-trip, amortised
/// behind a 2-second TTL cache so consecutive frames reuse the
/// previous result without a network call.
///
/// #830: thin wrapper over `crate::runtime::list_live_agents` (the
/// canonical helper consolidated when #830 became the fourth consumer
/// of this pattern). The wrapper survives so the call site keeps the
/// `tracing::warn!` observability hook that #827 added.
fn fetch_live_agents(home: &std::path::Path) -> Option<HashSet<String>> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    type Cache = (Option<Instant>, Option<HashSet<String>>);
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    const TTL: Duration = Duration::from_secs(2);

    let cache = CACHE.get_or_init(|| Mutex::new((None, None)));
    let Ok(mut guard) = cache.lock() else {
        return crate::runtime::list_live_agents(home);
    };
    if guard.0.is_some_and(|ts| ts.elapsed() < TTL) {
        return guard.1.clone();
    }
    let result = crate::runtime::list_live_agents(home);
    if result.is_none() {
        tracing::warn!(
            "#827: api::call(LIST) failed — active-indicator ghost filter degrading to identity"
        );
    }
    *guard = (Some(Instant::now()), result.clone());
    result
}

/// #827: drop assignees that aren't in the live runtime registry.
/// When `live` is `Some(set)`, retain only assignees in the set; when
/// `None` (api::call failed), keep all assignees so the indicator
/// degrades gracefully rather than reporting incorrect idleness.
///
/// Pure function — kept extracted from the render-site call so unit
/// tests can pin the contract without spinning up a ratatui frame.
///
fn filter_live_assignees<'a>(
    assignees: impl Iterator<Item = &'a str>,
    live: Option<&HashSet<String>>,
) -> HashSet<&'a str> {
    match live {
        Some(set) => assignees.filter(|name| set.contains(*name)).collect(),
        None => assignees.collect(),
    }
}

/// Format the "active:" footer. Pure function extracted so we can
/// pin deterministic ordering in a unit test — `HashSet` iteration
/// is non-deterministic, so a render path that joins it directly
/// visibly flickers between frames when the iteration order
/// reshuffles. The helper sorts before joining to lock a stable
/// left-to-right order independent of hash-bucket layout.
fn format_active_status(mut names: Vec<&str>) -> String {
    if names.is_empty() {
        "all idle".to_string()
    } else {
        names.sort_unstable();
        format!("active: {}", names.join(", "))
    }
}

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
                    crate::display_time::format_local_short(&d.created_at, None)
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
        BoardView::Tasks => "[T] TASKS | [f] fleet | [s] status | [m] monitor",
        BoardView::Fleet => "[t] tasks | [F] FLEET | [s] status | [m] monitor",
        BoardView::Status => "[t] tasks | [f] fleet | [S] STATUS | [m] monitor",
        BoardView::Monitor => "[t] tasks | [f] fleet | [s] status | [M] MONITOR",
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

    let columns = task_board_columns_viewport(items, inner.height as usize);

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
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            return;
        }
    }

    let col_areas = ratatui::layout::Layout::horizontal([
        Constraint::Ratio(1, 5),
        Constraint::Ratio(1, 5),
        Constraint::Ratio(1, 5),
        Constraint::Ratio(1, 5),
        Constraint::Ratio(1, 5),
    ])
    .split(inner);

    let col_titles = ["Backlog", "Ready", "Working", "Review", "Done"];
    let done_total = columns.get(4).map(|c| c.len()).unwrap_or(0);
    let done_visible = done_total.min(DONE_VISIBLE_MAX);
    let done_older = done_total.saturating_sub(DONE_VISIBLE_MAX);
    let col_title_strs: Vec<String> = col_titles
        .iter()
        .enumerate()
        .map(|(i, t)| {
            if i == 4 {
                format!(" {} ({}) ", t, done_total)
            } else {
                format!(" {} ({}) ", t, columns.get(i).map(|c| c.len()).unwrap_or(0))
            }
        })
        .collect();
    let col_colors = [
        Color::Gray,
        Color::Green,
        Color::Yellow,
        Color::Cyan,
        Color::DarkGray,
    ];

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

        let visible_tasks: &[&crate::tasks::Task] = if ci == 4 {
            &tasks[..done_visible]
        } else {
            tasks
        };
        let mut lines: Vec<Line> = Vec::new();
        for (ri, t) in visible_tasks.iter().enumerate() {
            let is_selected = is_active && ri == sel_row;
            let pri_badge = match t.priority {
                crate::task_events::TaskPriority::Urgent => "🔴",
                crate::task_events::TaskPriority::High => "🟠",
                crate::task_events::TaskPriority::Normal => "🔵",
                crate::task_events::TaskPriority::Low => "⚪",
            };
            let blocked = if t.status == crate::task_events::TaskStatus::Blocked {
                " 🔴"
            } else {
                ""
            };
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
        if ci == 4 && done_older > 0 {
            lines.push(Line::from(Span::styled(
                format!("── older ({done_older}) ──"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        frame.render_widget(Paragraph::new(lines), block_inner);
    }

    if inner.height > 2 && inner.width > 10 {
        // #827: cross-ref assignees against the daemon's live runtime
        // registry so ghost agents from disbanded teams (or any other
        // path that leaves a task owner string referencing a
        // no-longer-running instance) don't show up in the "active:"
        // indicator. `fetch_live_agents` returns `None` when the
        // daemon is offline; the filter degrades to the pre-#827
        // identity behavior in that case.
        let live = fetch_live_agents(home);
        let active_agents = filter_live_assignees(
            columns[2].iter().filter_map(|t| t.assignee.as_deref()),
            live.as_ref(),
        );
        let status_text = format_active_status(active_agents.into_iter().collect());
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

/// Max visible tasks in the Done column before collapsing into a summary line.
const DONE_VISIBLE_MAX: usize = 20;

/// Extra rows beyond viewport to keep sorted (absorbs off-by-one / border).
const PARTIAL_SORT_BUFFER: usize = 5;

/// Group tasks into 4 kanban columns: Backlog, Open, In Progress, Done.
/// Sorted by priority desc then created_at asc within each column.
/// Done column is sorted by updated_at desc (most recent first).
/// Cancelled tasks are excluded.
///
/// Fully sorts every column. Use [`task_board_columns_viewport`] in
/// per-frame render paths where only the first `viewport_rows` items
/// need to be in order.
pub fn task_board_columns(items: &[crate::tasks::Task]) -> [Vec<&crate::tasks::Task>; 5] {
    task_board_columns_viewport(items, usize::MAX)
}

/// Viewport-aware variant: only the first `viewport_rows + BUFFER`
/// items in each column are guaranteed sorted. Items beyond that
/// range are partitioned (correct side of the boundary) but in
/// arbitrary order.
pub fn task_board_columns_viewport(
    items: &[crate::tasks::Task],
    viewport_rows: usize,
) -> [Vec<&crate::tasks::Task>; 5] {
    let mut backlog: Vec<&crate::tasks::Task> = Vec::new();
    let mut ready: Vec<&crate::tasks::Task> = Vec::new();
    let mut working: Vec<&crate::tasks::Task> = Vec::new();
    let mut review: Vec<&crate::tasks::Task> = Vec::new();
    let mut done: Vec<&crate::tasks::Task> = Vec::new();

    for t in items {
        use crate::task_events::{TaskPriority, TaskStatus};
        match t.status {
            TaskStatus::Cancelled => {}
            TaskStatus::Backlog => backlog.push(t),
            TaskStatus::Open | TaskStatus::Blocked if t.priority == TaskPriority::Low => {
                backlog.push(t)
            }
            TaskStatus::Open | TaskStatus::Blocked => ready.push(t),
            TaskStatus::Claimed | TaskStatus::InProgress => working.push(t),
            TaskStatus::InReview | TaskStatus::Verified => review.push(t),
            TaskStatus::Done => done.push(t),
        }
    }

    let cap = viewport_rows.saturating_add(PARTIAL_SORT_BUFFER);

    fn pri_cmp(a: &&crate::tasks::Task, b: &&crate::tasks::Task) -> std::cmp::Ordering {
        let pri_ord = |p: crate::task_events::TaskPriority| -> u8 {
            match p {
                crate::task_events::TaskPriority::Urgent => 0,
                crate::task_events::TaskPriority::High => 1,
                crate::task_events::TaskPriority::Normal => 2,
                crate::task_events::TaskPriority::Low => 3,
            }
        };
        pri_ord(a.priority)
            .cmp(&pri_ord(b.priority))
            .then(a.created_at.cmp(&b.created_at))
    }

    fn partial_sort_by<T>(col: &mut [T], cap: usize, cmp: fn(&T, &T) -> std::cmp::Ordering) {
        if cap < col.len() {
            col.select_nth_unstable_by(cap, cmp);
            col[..cap].sort_by(cmp);
        } else {
            col.sort_by(cmp);
        }
    }

    partial_sort_by(&mut backlog, cap, pri_cmp);
    partial_sort_by(&mut ready, cap, pri_cmp);
    partial_sort_by(&mut working, cap, pri_cmp);
    working.sort_by(|a, b| {
        a.assignee
            .as_deref()
            .unwrap_or("")
            .cmp(b.assignee.as_deref().unwrap_or(""))
    });
    partial_sort_by(&mut review, cap, pri_cmp);
    let done_cap = cap.min(DONE_VISIBLE_MAX);
    fn done_cmp(a: &&crate::tasks::Task, b: &&crate::tasks::Task) -> std::cmp::Ordering {
        b.updated_at.cmp(&a.updated_at)
    }
    partial_sort_by(&mut done, done_cap, done_cmp);

    [backlog, ready, working, review, done]
}

/// Number of selectable (visible) tasks in a column.
/// Done column is capped at `DONE_VISIBLE_MAX`; others return full length.
pub fn selectable_len(columns: &[Vec<&crate::tasks::Task>; 5], col: usize) -> usize {
    let len = columns.get(col).map(|c| c.len()).unwrap_or(0);
    if col == 4 {
        len.min(DONE_VISIBLE_MAX)
    } else {
        len
    }
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
        let parsed_status: crate::task_events::TaskStatus =
            serde_json::from_value(serde_json::Value::String(status.to_string())).unwrap();
        let parsed_priority: crate::task_events::TaskPriority =
            serde_json::from_value(serde_json::Value::String(priority.to_string())).unwrap();
        crate::tasks::Task {
            id: title.to_string(),
            title: title.to_string(),
            description: String::new(),
            status: parsed_status,
            priority: parsed_priority,
            assignee: None,
            routed_to: None,
            created_by: String::new(),
            depends_on: Vec::new(),
            result: None,
            created_at: created_at.to_string(),
            updated_at: String::new(),
            due_at: None,
            branch: None,
            started_at: None,
            eta_secs: None,
            auto_release_on_verdict: None,
            tags: vec![],
            parent_id: None,
            metadata: std::collections::BTreeMap::new(),
        }
    }

    fn make_done_task(title: &str, updated_at: &str) -> crate::tasks::Task {
        let mut t = make_task(title, "done", "normal", "2026-01-01");
        t.updated_at = updated_at.to_string();
        t
    }

    #[test]
    fn done_column_sorted_by_updated_at_desc() {
        let tasks = vec![
            make_done_task("old", "2026-01-01T00:00:00Z"),
            make_done_task("mid", "2026-01-15T00:00:00Z"),
            make_done_task("new", "2026-01-30T00:00:00Z"),
        ];
        let [_, _, _, _, done] = task_board_columns(&tasks);
        assert_eq!(done[0].title, "new");
        assert_eq!(done[1].title, "mid");
        assert_eq!(done[2].title, "old");
    }

    #[test]
    fn done_column_selectable_len_capped() {
        let tasks: Vec<crate::tasks::Task> = (0..30)
            .map(|i| {
                make_done_task(
                    &format!("task-{i}"),
                    &format!("2026-01-{:02}T00:00:00Z", (i % 28) + 1),
                )
            })
            .collect();
        let columns = task_board_columns(&tasks);
        assert_eq!(columns[4].len(), 30, "full done column has 30 tasks");
        assert_eq!(
            selectable_len(&columns, 4),
            DONE_VISIBLE_MAX,
            "selectable len capped at DONE_VISIBLE_MAX"
        );
    }

    #[test]
    fn done_column_selectable_len_uncapped_when_small() {
        let tasks = vec![
            make_done_task("a", "2026-01-01T00:00:00Z"),
            make_done_task("b", "2026-01-02T00:00:00Z"),
        ];
        let columns = task_board_columns(&tasks);
        assert_eq!(selectable_len(&columns, 4), 2);
    }

    #[test]
    fn other_columns_selectable_len_not_capped() {
        let tasks: Vec<crate::tasks::Task> = (0..30)
            .map(|i| make_task(&format!("task-{i}"), "open", "high", "2026-01-01"))
            .collect();
        let columns = task_board_columns(&tasks);
        assert_eq!(
            selectable_len(&columns, 1),
            30,
            "open column should not be capped"
        );
    }

    // ── #827 active-indicator ghost filter ──

    fn make_live(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    /// C1 RED: live registry known + one ghost name in assignees →
    /// ghost dropped, live names retained.
    #[test]
    fn filter_drops_ghost_when_live_known() {
        let live = make_live(&["alpha", "beta"]);
        let assignees = ["alpha", "beta", "ghost"];
        let kept = filter_live_assignees(assignees.iter().copied(), Some(&live));
        assert_eq!(
            kept,
            HashSet::from(["alpha", "beta"]),
            "ghost-agent must be filtered out when live registry is known"
        );
    }

    /// C1 RED: every assignee is a ghost → caller renders "all idle".
    #[test]
    fn filter_returns_empty_when_all_ghosts() {
        let live = make_live(&["xenon"]);
        let assignees = ["alpha", "beta"];
        let kept = filter_live_assignees(assignees.iter().copied(), Some(&live));
        assert!(
            kept.is_empty(),
            "all-ghost assignees must produce empty set (caller renders 'all idle'); got: {kept:?}"
        );
    }

    /// C1 GREEN (also passes at C1 with identity stub): graceful
    /// fallback when api::call(LIST) fails — keep all assignees
    /// rather than misleadingly report "all idle".
    #[test]
    fn filter_keeps_all_when_live_is_none() {
        let assignees = ["alpha", "ghost"];
        let kept = filter_live_assignees(assignees.iter().copied(), None);
        assert_eq!(
            kept,
            HashSet::from(["alpha", "ghost"]),
            "daemon-offline (live=None) must preserve current behavior — show all assignees"
        );
    }

    /// C1 GREEN: empty assignees → empty result regardless of `live`.
    /// Edge case; locks the contract so a future filter refactor can't
    /// accidentally inject something into the empty case.
    #[test]
    fn filter_handles_empty_assignees() {
        let live = make_live(&["alpha"]);
        let kept = filter_live_assignees(std::iter::empty::<&str>(), Some(&live));
        assert!(kept.is_empty(), "empty input must produce empty output");
    }

    /// RED: HashSet iteration order is non-deterministic, so a render
    /// path that joins names straight from the set visibly flickers
    /// between frames. Pin the contract: same input set → same output
    /// string, regardless of caller's iteration order.
    #[test]
    fn format_active_status_sorts_names_alphabetically() {
        let result = format_active_status(vec!["zebra", "alpha", "mike"]);
        assert_eq!(
            result, "active: alpha, mike, zebra",
            "names must render in stable alphabetical order — otherwise the active-indicator flickers across frames"
        );
    }

    /// Edge case: zero live names → "all idle" sentinel.
    #[test]
    fn format_active_status_returns_all_idle_for_empty() {
        let result = format_active_status(vec![]);
        assert_eq!(result, "all idle");
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
        let [backlog, ready, working, _review, done] = task_board_columns(&tasks);
        assert_eq!(backlog.len(), 2, "open+low and blocked+low → backlog");
        assert_eq!(ready.len(), 1, "open+high → open");
        assert_eq!(working.len(), 1, "claimed → in progress");
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
        let [_, ready, _, _, _] = task_board_columns(&tasks);
        assert_eq!(ready[0].title, "urgent-new", "urgent first");
        assert_eq!(ready[1].title, "high-old", "high second");
        assert_eq!(ready[2].title, "normal-old", "normal oldest third");
        assert_eq!(ready[3].title, "normal-new", "normal newest last");
    }

    #[test]
    fn viewport_partial_sort_matches_full_sort_in_visible_range() {
        let tasks: Vec<crate::tasks::Task> = (0..50)
            .map(|i| {
                let pri = match i % 4 {
                    0 => "urgent",
                    1 => "high",
                    2 => "normal",
                    _ => "low",
                };
                make_task(
                    &format!("task-{i:03}"),
                    "open",
                    pri,
                    // unique created_at per task (avoids tie-breaking instability)
                    &format!("2026-{:02}-{:02}", (i / 28) + 1, (i % 28) + 1),
                )
            })
            .collect();

        let full = task_board_columns(&tasks);
        let partial = task_board_columns_viewport(&tasks, 10);
        let cap = 10 + PARTIAL_SORT_BUFFER;

        for col in 0..4 {
            assert_eq!(
                full[col].len(),
                partial[col].len(),
                "column {col} length must match"
            );
            let visible = cap.min(full[col].len());
            for i in 0..visible {
                assert_eq!(
                    full[col][i].id, partial[col][i].id,
                    "column {col} row {i} must match between full and partial sort"
                );
            }
        }
    }

    #[test]
    fn viewport_partial_sort_done_column_top_n_correct() {
        let tasks: Vec<crate::tasks::Task> = (0..40)
            .map(|i| {
                make_done_task(
                    &format!("done-{i:03}"),
                    // unique updated_at per task
                    &format!("2026-{:02}-{:02}T12:00:00Z", (i / 28) + 1, (i % 28) + 1),
                )
            })
            .collect();

        let full = task_board_columns(&tasks);
        let partial = task_board_columns_viewport(&tasks, 10);

        assert_eq!(full[3].len(), partial[3].len());
        let visible = DONE_VISIBLE_MAX.min(full[3].len());
        for i in 0..visible {
            assert_eq!(
                full[3][i].id, partial[3][i].id,
                "done column row {i}: partial sort must match full sort in visible range"
            );
        }
    }

    fn render_task_board_to_string(mode: &crate::app::TaskBoardMode) -> String {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal =
            ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
        let tasks = vec![crate::tasks::Task {
            id: "t-1".into(),
            title: "test task".into(),
            description: String::new(),
            status: crate::task_events::TaskStatus::Open,
            priority: crate::task_events::TaskPriority::Normal,
            assignee: None,
            routed_to: None,
            depends_on: Vec::new(),
            result: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            created_by: "test".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            due_at: None,
            branch: None,
            started_at: None,
            eta_secs: None,
            auto_release_on_verdict: None,
            tags: vec![],
            parent_id: None,
            metadata: std::collections::BTreeMap::new(),
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
