//! Border grid system: joined box-drawing across shared pane edges.

use ratatui::style::Style;
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

use super::core_render::PaneBorderInfo;

pub(super) const DIR_N: u8 = 0b0001;
pub(super) const DIR_E: u8 = 0b0010;
pub(super) const DIR_S: u8 = 0b0100;
pub(super) const DIR_W: u8 = 0b1000;

#[derive(Clone, Copy, Default)]
pub(super) struct BorderCell {
    pub(super) mask: u8,
    pub(super) style: Style,
    pub(super) priority: u8,
}

pub(super) fn border_char(mask: u8) -> Option<char> {
    let c = match mask {
        0 => return None,
        m if m == DIR_N => '│',
        m if m == DIR_S => '│',
        m if m == DIR_E => '─',
        m if m == DIR_W => '─',
        m if m == DIR_N | DIR_S => '│',
        m if m == DIR_E | DIR_W => '─',
        m if m == DIR_N | DIR_E => '└',
        m if m == DIR_N | DIR_W => '┘',
        m if m == DIR_S | DIR_E => '┌',
        m if m == DIR_S | DIR_W => '┐',
        m if m == DIR_N | DIR_S | DIR_E => '├',
        m if m == DIR_N | DIR_S | DIR_W => '┤',
        m if m == DIR_N | DIR_E | DIR_W => '┴',
        m if m == DIR_S | DIR_E | DIR_W => '┬',
        m if m == DIR_N | DIR_E | DIR_S | DIR_W => '┼',
        _ => return None,
    };
    Some(c)
}

/// Add a pane's 4 edges to the grid.
pub(super) fn add_pane_borders(
    cells: &mut std::collections::HashMap<(u16, u16), BorderCell>,
    info: &PaneBorderInfo,
) {
    let area = info.area;
    if area.width < 2 || area.height < 2 {
        return;
    }
    let x0 = area.x;
    let y0 = area.y;
    let x1 = area.x + area.width - 1;
    let y1 = area.y + area.height - 1;

    let merge = |cells: &mut std::collections::HashMap<(u16, u16), BorderCell>,
                 x: u16,
                 y: u16,
                 mask: u8| {
        let slot = cells.entry((x, y)).or_default();
        slot.mask |= mask;
        if info.priority > slot.priority {
            slot.style = info.border_style;
            slot.priority = info.priority;
        }
    };

    for x in x0..=x1 {
        let mut top = 0u8;
        let mut bot = 0u8;
        if x > x0 {
            top |= DIR_W;
            bot |= DIR_W;
        }
        if x < x1 {
            top |= DIR_E;
            bot |= DIR_E;
        }
        if x == x0 || x == x1 {
            top |= DIR_S;
            bot |= DIR_N;
        }
        merge(cells, x, y0, top);
        merge(cells, x, y1, bot);
    }
    if y1 > y0 + 1 {
        for y in (y0 + 1)..y1 {
            merge(cells, x0, y, DIR_N | DIR_S);
            merge(cells, x1, y, DIR_N | DIR_S);
        }
    }
}

pub(super) fn render_border_grid(frame: &mut Frame, infos: &[PaneBorderInfo]) {
    let mut cells: std::collections::HashMap<(u16, u16), BorderCell> =
        std::collections::HashMap::new();
    for info in infos {
        add_pane_borders(&mut cells, info);
    }
    let buf = frame.buffer_mut();
    let buf_area = buf.area;
    for ((x, y), cell) in cells {
        if x < buf_area.x
            || x >= buf_area.x + buf_area.width
            || y < buf_area.y
            || y >= buf_area.y + buf_area.height
        {
            continue;
        }
        if let Some(ch) = border_char(cell.mask) {
            let b = &mut buf[(x, y)];
            b.set_char(ch);
            b.set_style(cell.style);
        }
    }
}

/// Overlay each pane's title on its top border row.
pub(super) fn render_pane_titles(frame: &mut Frame, infos: &[PaneBorderInfo]) {
    let buf = frame.buffer_mut();
    let buf_area = buf.area;
    for info in infos {
        let area = info.area;
        if area.width < 3 || area.height == 0 {
            continue;
        }
        let y = area.y;
        let last_usable_x = area.x.saturating_add(area.width).saturating_sub(1);
        let buf_right = buf_area.x.saturating_add(buf_area.width);
        let buf_bottom = buf_area.y.saturating_add(buf_area.height);
        let mut x = area.x.saturating_add(1);
        for (segment, style) in &info.title_segments {
            for g in segment.chars() {
                let w = u16::try_from(UnicodeWidthChar::width(g).unwrap_or(0)).unwrap_or(u16::MAX);
                if w == 0 {
                    continue;
                }
                if x.saturating_add(w) > last_usable_x {
                    break;
                }
                if x >= buf_right || y >= buf_bottom {
                    break;
                }
                let cell = &mut buf[(x, y)];
                cell.set_char(g);
                cell.set_style(*style);
                for off in 1..w {
                    let tx = x.saturating_add(off);
                    if tx >= buf_right {
                        break;
                    }
                    let trail = &mut buf[(tx, y)];
                    trail.set_char(' ');
                    trail.set_style(*style);
                }
                x = x.saturating_add(w);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use std::collections::HashMap;

    fn info(x: u16, y: u16, w: u16, h: u16) -> PaneBorderInfo {
        PaneBorderInfo {
            area: Rect::new(x, y, w, h),
            border_style: Style::default(),
            title_segments: Vec::new(),
            priority: 1,
        }
    }

    #[test]
    fn border_char_corner_and_junction_masks() {
        assert_eq!(border_char(0), None);
        assert_eq!(border_char(DIR_S | DIR_E), Some('┌'));
        assert_eq!(border_char(DIR_S | DIR_W), Some('┐'));
        assert_eq!(border_char(DIR_N | DIR_E), Some('└'));
        assert_eq!(border_char(DIR_N | DIR_W), Some('┘'));
        assert_eq!(border_char(DIR_E | DIR_W), Some('─'));
        assert_eq!(border_char(DIR_N | DIR_S), Some('│'));
        assert_eq!(border_char(DIR_N | DIR_S | DIR_E), Some('├'));
        assert_eq!(border_char(DIR_N | DIR_S | DIR_W), Some('┤'));
        assert_eq!(border_char(DIR_S | DIR_E | DIR_W), Some('┬'));
        assert_eq!(border_char(DIR_N | DIR_E | DIR_W), Some('┴'));
        assert_eq!(border_char(DIR_N | DIR_S | DIR_E | DIR_W), Some('┼'));
    }

    #[test]
    fn single_pane_renders_outer_frame() {
        let mut cells: HashMap<(u16, u16), BorderCell> = HashMap::new();
        add_pane_borders(&mut cells, &info(0, 0, 10, 5));
        assert_eq!(border_char(cells[&(0, 0)].mask), Some('┌'));
        assert_eq!(border_char(cells[&(9, 0)].mask), Some('┐'));
        assert_eq!(border_char(cells[&(0, 4)].mask), Some('└'));
        assert_eq!(border_char(cells[&(9, 4)].mask), Some('┘'));
        assert_eq!(border_char(cells[&(5, 0)].mask), Some('─'));
        assert_eq!(border_char(cells[&(5, 4)].mask), Some('─'));
        assert_eq!(border_char(cells[&(0, 2)].mask), Some('│'));
        assert_eq!(border_char(cells[&(9, 2)].mask), Some('│'));
    }

    #[test]
    fn adjacent_vertical_panes_produce_t_junctions() {
        let mut cells: HashMap<(u16, u16), BorderCell> = HashMap::new();
        add_pane_borders(&mut cells, &info(0, 0, 10, 5));
        add_pane_borders(&mut cells, &info(9, 0, 11, 5));
        assert_eq!(
            border_char(cells[&(9, 0)].mask),
            Some('┬'),
            "top of shared column must be ┬, not two stacked ┐┌"
        );
        assert_eq!(border_char(cells[&(9, 4)].mask), Some('┴'));
        assert_eq!(border_char(cells[&(9, 2)].mask), Some('│'));
        assert_eq!(border_char(cells[&(0, 0)].mask), Some('┌'));
        assert_eq!(border_char(cells[&(19, 0)].mask), Some('┐'));
    }

    #[test]
    fn four_way_grid_produces_cross_junction() {
        let mut cells: HashMap<(u16, u16), BorderCell> = HashMap::new();
        add_pane_borders(&mut cells, &info(0, 0, 10, 5));
        add_pane_borders(&mut cells, &info(9, 0, 11, 5));
        add_pane_borders(&mut cells, &info(0, 4, 10, 6));
        add_pane_borders(&mut cells, &info(9, 4, 11, 6));
        assert_eq!(
            border_char(cells[&(9, 4)].mask),
            Some('┼'),
            "4-way junction must be ┼"
        );
        assert_eq!(
            border_char(cells[&(0, 4)].mask),
            Some('├'),
            "left-edge T must be ├"
        );
        assert_eq!(
            border_char(cells[&(19, 4)].mask),
            Some('┤'),
            "right-edge T must be ┤"
        );
    }

    #[test]
    fn higher_priority_wins_shared_cell_style() {
        let mut a = info(0, 0, 10, 5);
        a.priority = 5;
        a.border_style = Style::default().fg(Color::Magenta);
        let b = info(9, 0, 11, 5);
        let mut cells: HashMap<(u16, u16), BorderCell> = HashMap::new();
        add_pane_borders(&mut cells, &b);
        add_pane_borders(&mut cells, &a);
        let shared = cells[&(9, 2)];
        assert_eq!(shared.priority, 5);
        assert_eq!(shared.style.fg, Some(Color::Magenta));
    }
}
