//! Overlay widgets: menu, rename, tab list, confirm, help, command palette.

use crate::app::MenuItem;
use crate::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

/// Clamp a desired overlay dimension by the available space minus padding.
pub(super) fn clamp_overlay_dim(desired: u16, available: u16, pad: u16) -> u16 {
    desired.min(available.saturating_sub(pad))
}

/// Centred coordinate for an overlay of `overlay_dim` within `area_dim`.
pub(super) fn center_overlay(area_dim: u16, overlay_dim: u16) -> u16 {
    area_dim.saturating_sub(overlay_dim) / 2
}

/// Compute a centred overlay `Rect` with saturating arithmetic.
pub fn centered_overlay_rect(
    area: Rect,
    content_h: u16,
    content_w: u16,
    h_pad: u16,
    w_pad: u16,
) -> Rect {
    let height = clamp_overlay_dim(content_h, area.height, h_pad);
    let width = clamp_overlay_dim(content_w, area.width, w_pad);
    let x = center_overlay(area.width, width);
    let y = center_overlay(area.height, height);
    Rect::new(x, y, width, height)
}

/// #2050 simplify PR-C (③): the shared body of the *titled* overlay popups —
/// `Clear` the pre-computed `area`, draw a bordered block with a bold `title` in
/// `color`, and return the inner content rect. Each caller computes its own `area`
/// (the sizes differ) then fills the returned rect. Excludes `render_confirm`,
/// which is title-less (structurally different).
pub(super) fn render_titled_popup<'a>(
    frame: &mut Frame,
    area: Rect,
    color: Color,
    title: impl Into<std::borrow::Cow<'a, str>>,
) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            title.into(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Render a centered (80%) overlay frame with border and title. Returns the inner area.
pub(super) fn render_overlay_frame(frame: &mut Frame, color: Color, title: &str) -> Rect {
    let area = frame.area();
    let h = (area.height * 80 / 100).max(10);
    let w = (area.width * 80 / 100).max(40);
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let oa = Rect::new(x, y, w, h);
    render_titled_popup(frame, oa, color, title)
}

pub fn render_menu(frame: &mut Frame, items: &[MenuItem], selected: usize) {
    let area = frame.area();
    let item_count = u16::try_from(items.len()).unwrap_or(u16::MAX);
    let menu_area = centered_overlay_rect(area, item_count.saturating_add(4), 50, 2, 4);
    let inner = render_titled_popup(
        frame,
        menu_area,
        Color::Cyan,
        " New Tab (Enter to select, Esc to cancel) ",
    );
    let lines: Vec<Line> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let style = if i == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let prefix = if i == selected { "> " } else { "  " };
            Line::from(Span::styled(format!("{prefix}{}", item.label), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_rename(frame: &mut Frame, input: &str) {
    let area = frame.area();
    let w = clamp_overlay_dim(40, area.width, 4);
    let x = center_overlay(area.width, w);
    let y = (area.height / 2).saturating_sub(1);
    let ra = Rect::new(x, y, w, 3);
    let inner = render_titled_popup(frame, ra, Color::Yellow, " Rename (Enter, Esc) ");
    frame.render_widget(
        Paragraph::new(input.to_string()).style(Style::default().fg(Color::White)),
        inner,
    );
    let cursor_x = inner.x + input.width() as u16;
    if cursor_x < inner.x + inner.width {
        frame.set_cursor_position(ratatui::layout::Position::new(cursor_x, inner.y));
    }
}

pub fn render_tab_list(frame: &mut Frame, layout: &Layout, selected: usize) {
    let area = frame.area();
    let content_h = (layout.tabs.len() as u16).saturating_add(4);
    let la = centered_overlay_rect(area, content_h, 50, 2, 4);
    let inner = render_titled_popup(frame, la, Color::Cyan, " Windows (Enter, Esc) ");
    let lines: Vec<Line> = layout
        .tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let is_sel = i == selected;
            let style = if is_sel {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let marker = if i == layout.active { "*" } else { " " };
            let pc = tab.root().pane_count();
            Line::from(vec![
                Span::styled(format!("{marker} {i}: "), style),
                Span::styled(tab.name.as_str(), style),
                Span::styled(
                    format!("  ({pc} pane{s})", s = if pc > 1 { "s" } else { "" }),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_move_pane_target(
    frame: &mut Frame,
    layout: &Layout,
    selected: usize,
    source_tab_idx: usize,
    split_dir: crate::layout::SplitDir,
) {
    let area = frame.area();
    let list_len = (layout.tabs.len() as u16).saturating_add(1);
    let content_h = list_len.saturating_add(4);
    let la = centered_overlay_rect(area, content_h, 54, 2, 4);
    let inner = render_titled_popup(
        frame,
        la,
        Color::Magenta,
        format!(" Move pane to... (Split: {:?}) (Tab to toggle) ", split_dir),
    );

    let mut lines: Vec<Line> = Vec::with_capacity(list_len.into());
    for (i, tab) in layout.tabs.iter().enumerate() {
        let is_sel = i == selected;
        let is_source = i == source_tab_idx;
        let style = if is_sel {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD)
        } else if is_source {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        let marker = if is_source { "(source)" } else { "" };
        let pc = tab.root().pane_count();
        lines.push(Line::from(vec![
            Span::styled(format!(" {i}: "), style),
            Span::styled(tab.name.as_str(), style),
            Span::styled(
                format!(
                    "  ({pc} pane{s}) {marker}",
                    s = if pc > 1 { "s" } else { "" }
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    let new_sel = selected == layout.tabs.len();
    let style = if new_sel {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green)
    };
    lines.push(Line::from(vec![Span::styled(" [+] New tab", style)]));

    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_confirm(frame: &mut Frame, message: &str) {
    let area = frame.area();
    let content_w = u16::try_from(message.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4);
    let w = clamp_overlay_dim(content_w, area.width, 4);
    let x = center_overlay(area.width, w);
    let y = (area.height / 2).saturating_sub(1);
    let ca = Rect::new(x, y, w, 3);
    frame.render_widget(Clear, ca);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));
    let inner = block.inner(ca);
    frame.render_widget(block, ca);
    frame.render_widget(
        Paragraph::new(Span::styled(
            message,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        inner,
    );
}

pub fn render_help(frame: &mut Frame) {
    let help = vec![
        "",
        "  Tab Management",
        "    Ctrl+B c       New tab",
        "    Ctrl+B n / p   Next / previous tab",
        "    Ctrl+B l       Last used tab",
        "    Ctrl+B 0-9     Go to tab N",
        "    Ctrl+B &       Close tab",
        "    Ctrl+B ,       Rename tab",
        "    Ctrl+B w       List all tabs",
        "",
        "  Pane Management",
        "    Ctrl+B \"       Split horizontal",
        "    Ctrl+B %       Split vertical",
        "    Ctrl+B o       Cycle pane focus",
        "    Ctrl+B arrows  Directional focus",
        "    Ctrl+B A-arrow Resize pane (Alt-arrow)",
        "    Ctrl+B H/J/K/L Resize pane (portable)",
        "    Drag border    Resize pane",
        "    Drag title     Swap pane position",
        "    Drag → tab bar Move pane across tabs (drop on tab or [+])",
        "    Ctrl+B x       Close pane",
        "    Ctrl+B z       Toggle zoom",
        "    Ctrl+B Space   Next layout preset",
        "    Ctrl+B .       Rename pane",
        "    Ctrl+B !       Move pane to another tab (menu)",
        "",
        "  Scroll",
        "    Mouse wheel    Scroll focused pane",
        "    Ctrl+B [       Keyboard scroll mode",
        "    Shift+drag     Select text (native)",
        "",
        "  Selection & Copy",
        "    Drag           Select & auto-copy (copy-on-select, default)",
        "    Ctrl+B e       Toggle copy-on-select / explicit-copy mode",
        "    Cmd+C, Ctrl+Shift+C   Copy selection (explicit-copy mode)",
        "",
        "  Panels & Commands",
        "    Ctrl+B :       Command palette",
        "      :spawn <n> [backend]  New tab",
        "      :vsplit <n> [backend] V-split",
        "      :hsplit <n> [backend] H-split",
        "      :layout [name]        Arrange panes",
        "      :kill <name>          Kill agent",
        "      :restart [name]       Restart agent",
        "      :send <to> <msg>      Send message",
        "      :broadcast <msg>      Broadcast",
        "      :status               Log agent states",
        "    Ctrl+B D       Decisions panel",
        "    Ctrl+B T       Task board",
        "",
        "  Other",
        "    Ctrl+J         Newline (no submit, works everywhere)",
        "    Shift+Enter    Newline (requires modern terminal)",
        "    Ctrl+B Ctrl+B  Send Ctrl+B to pane",
        "    Ctrl+B ~       Scratch shell (Esc to close)",
        "    Ctrl+B d       Detach (exit)",
        "    Ctrl+B ?       This help",
        "",
        "  Press any key to close",
    ];
    let area = frame.area();
    let h = (help.len() as u16 + 2).min(area.height.saturating_sub(2));
    let content_w = help.iter().map(|l| l.len() as u16).max().unwrap_or(48) + 2;
    let w = content_w.min(area.width.saturating_sub(4));
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let ha = Rect::new(x, y, w, h);
    let inner = render_titled_popup(frame, ha, Color::Yellow, " Keybindings ");
    let lines: Vec<Line> = help
        .iter()
        .map(|l| Line::from(Span::styled(*l, Style::default().fg(Color::White))))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

pub fn render_scroll_indicator(frame: &mut Frame, offset: usize) {
    let area = frame.area();
    let s = format!(" [scroll] line +{offset} | j/k PgUp/PgDn | q exit ");
    let w = s.len() as u16;
    let ba = Rect::new(area.width.saturating_sub(w), 0, w, 1);
    frame.render_widget(
        Paragraph::new(Span::styled(
            s,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        ba,
    );
}

/// #t-5: command palette with prefix-match completion. Renders the `:input` line
/// plus the precomputed [`Completion`] (the same one the key handler uses, so the
/// highlight always matches what Tab completes) with the `selected` candidate
/// highlighted, modeled on [`render_menu`]. The candidate rows are command
/// keywords while typing the command, argument values once past it, or a single
/// dim usage hint for a free-form argument (not selectable).
pub fn render_command_palette(
    frame: &mut Frame,
    input: &str,
    selected: usize,
    completion: &crate::app::commands::Completion,
) {
    use crate::app::commands::Completion;
    let area = frame.area();

    // Display rows for the candidate area. A usage hint is informational (not
    // selectable), so it never takes the highlight.
    let rows: Vec<String> = match completion {
        Completion::Keyword(specs) => specs
            .iter()
            .map(|s| format!("{:<26} {}", s.usage, s.desc))
            .collect(),
        Completion::Values(values) => values.clone(),
        Completion::UsageHint(usage) => vec![format!("usage: {usage}")],
    };
    let selectable = !matches!(completion, Completion::UsageHint(_));

    // Window the list to the popup; beyond it the operator narrows by typing. The
    // highlight uses the SAME `clamp_selected` the key handler / `tab_complete`
    // use, so the highlighted row is always the candidate Tab will complete — even
    // when `selected` ran past the window (>MAX_PALETTE_ROWS candidates).
    let shown = rows.len().min(crate::app::commands::MAX_PALETTE_ROWS);
    let sel = completion.clamp_selected(selected);

    let w = 64u16.min(area.width.saturating_sub(4));
    let x = (area.width.saturating_sub(w)) / 2;
    // 1 input line + `shown` candidate rows + 2 border rows, clamped to the screen.
    let height = u16::try_from(shown + 3)
        .unwrap_or(u16::MAX)
        .min(area.height);
    // Anchor at ~1/3 down, but pull up so a tall popup (up to MAX_PALETTE_ROWS
    // value candidates) never spills past the bottom edge on a short terminal.
    let y = (area.height / 3).min(area.height.saturating_sub(height));
    let ra = Rect::new(x, y, w, height);
    let inner = render_titled_popup(frame, ra, Color::Cyan, " : Command — Tab ↑↓ Enter Esc ");

    let mut lines: Vec<Line> = Vec::with_capacity(shown + 1);
    lines.push(Line::from(Span::styled(
        format!(":{input}"),
        Style::default().fg(Color::White),
    )));
    for (i, row) in rows.iter().take(shown).enumerate() {
        let highlighted = selectable && i == sel;
        let style = if highlighted {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let prefix = if highlighted { "> " } else { "  " };
        lines.push(Line::from(Span::styled(format!("{prefix}{row}"), style)));
    }
    frame.render_widget(Paragraph::new(lines), inner);

    // Cursor on the input line, just after `:input`.
    let cursor_x = inner.x + 1 + input.width() as u16;
    if cursor_x < inner.x + inner.width {
        frame.set_cursor_position(ratatui::layout::Position::new(cursor_x, inner.y));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn clamp_overlay_dim_saturates_on_zero_area() {
        assert_eq!(clamp_overlay_dim(40, 0, 4), 0);
        assert_eq!(clamp_overlay_dim(40, 1, 4), 0);
        assert_eq!(clamp_overlay_dim(40, 4, 4), 0);
        assert_eq!(clamp_overlay_dim(40, 5, 4), 1);
    }

    #[test]
    fn clamp_overlay_dim_normal_case_returns_min_of_desired_and_available_minus_pad() {
        assert_eq!(clamp_overlay_dim(40, 200, 4), 40);
        assert_eq!(clamp_overlay_dim(200, 50, 4), 46);
        assert_eq!(clamp_overlay_dim(46, 50, 4), 46);
    }

    #[test]
    fn center_overlay_saturates_when_overlay_exceeds_area() {
        assert_eq!(center_overlay(0, 10), 0);
        assert_eq!(center_overlay(20, 50), 0);
        assert_eq!(center_overlay(100, 50), 25);
    }

    #[test]
    fn centered_overlay_rect_tiny_terminal_does_not_panic() {
        let r0 = centered_overlay_rect(Rect::new(0, 0, 0, 0), 10, 50, 2, 4);
        assert_eq!((r0.width, r0.height), (0, 0));

        let r1 = centered_overlay_rect(Rect::new(0, 0, 1, 1), 10, 50, 2, 4);
        assert_eq!((r1.width, r1.height), (0, 0));

        let r2 = centered_overlay_rect(Rect::new(0, 0, 4, 2), 10, 50, 2, 4);
        assert_eq!((r2.width, r2.height), (0, 0));
    }

    #[test]
    fn centered_overlay_rect_centers_within_area() {
        let r = centered_overlay_rect(Rect::new(0, 0, 100, 40), 10, 50, 2, 4);
        assert_eq!((r.x, r.y, r.width, r.height), (25, 15, 50, 10));
    }

    fn palette_text_sel(
        input: &str,
        selected: usize,
        completion: &crate::app::commands::Completion,
    ) -> String {
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_command_palette(frame, input, selected, completion))
            .unwrap();
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

    fn palette_text(input: &str, completion: &crate::app::commands::Completion) -> String {
        palette_text_sel(input, 0, completion)
    }

    /// #t-5 UX smoke: the palette lists ALL commands on empty input (the operator's
    /// "what commands exist?" discovery), and prefix-filters to matching keywords.
    #[test]
    fn command_palette_lists_all_on_open_and_prefix_filters() {
        use crate::app::commands::{matching_specs, Completion};
        // Empty input → list every command.
        let all = palette_text("", &Completion::Keyword(matching_specs("")));
        assert!(
            all.contains("spawn") && all.contains("config") && all.contains("layout"),
            "empty input must list all commands:\n{all}"
        );
        // Prefix narrows to matching keyword(s) only.
        let co = palette_text("co", &Completion::Keyword(matching_specs("co")));
        assert!(co.contains("config"), "`co` must show config:\n{co}");
        assert!(
            !co.contains("spawn") && !co.contains("layout"),
            "`co` must hide non-matching commands:\n{co}"
        );
    }

    /// #t-… param-value completion render: a `Values` completion lists the dynamic
    /// candidates, and a `UsageHint` (free-form argument) renders the dim hint line
    /// instead — directly answering the operator's "what do I put here?".
    #[test]
    fn command_palette_renders_values_and_usage_hint() {
        use crate::app::commands::Completion;
        // Value list (e.g. `:spawn foo <backend>`).
        let vals = palette_text(
            "spawn foo ",
            &Completion::Values(vec!["claude".to_string(), "codex".to_string()]),
        );
        assert!(
            vals.contains("claude") && vals.contains("codex"),
            "value completion must list the candidates:\n{vals}"
        );
        // Free-form argument → usage hint, no value list.
        let hint = palette_text("set key ", &Completion::UsageHint("set <key> <value>"));
        assert!(
            hint.contains("usage:") && hint.contains("set <key> <value>"),
            "free-form arg must show the usage hint:\n{hint}"
        );
    }

    /// r4 regression: with >MAX_PALETTE_ROWS candidates the popup windows the list
    /// and the highlight (`> `) lands on the last VISIBLE row — never an off-screen
    /// candidate — so it matches what `tab_complete` completes. RED before the fix
    /// (render clamped the highlight to row 11 while Tab used the raw `selected`).
    #[test]
    fn command_palette_highlight_stays_within_visible_window() {
        use crate::app::commands::{Completion, MAX_PALETTE_ROWS};
        let values: Vec<String> = (0..20).map(|i| format!("agent{i:02}")).collect();
        let comp = Completion::Values(values);
        // `selected` ran past the window (e.g. Down held). The highlight is the
        // last visible candidate (agent11), and no off-window candidate renders.
        let out = palette_text_sel("kill ", 14, &comp);
        assert!(
            out.contains("> agent11"),
            "highlight must be the last visible candidate (agent11):\n{out}"
        );
        assert!(
            !out.contains("agent14") && !out.contains("agent19"),
            "off-window candidates must not render:\n{out}"
        );
        // At most MAX_PALETTE_ROWS candidate rows are shown.
        let shown_rows = out.matches("agent").count();
        assert!(
            shown_rows <= MAX_PALETTE_ROWS,
            "windowed to <= {MAX_PALETTE_ROWS} rows, got {shown_rows}:\n{out}"
        );
    }
}
