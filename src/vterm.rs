//! Virtual terminal — wraps alacritty_terminal for screen state tracking.
//! Processes raw PTY output and can dump current screen as ANSI for reconnection.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{self, Config};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

#[derive(Clone)]
struct NoopListener;
impl EventListener for NoopListener {
    fn send_event(&self, _event: Event) {}
}

struct VTermSize {
    cols: u16,
    rows: u16,
}
impl Dimensions for VTermSize {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

pub struct VTerm {
    term: term::Term<NoopListener>,
    processor: Processor,
    cols: u16,
    rows: u16,
}

impl VTerm {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = VTermSize { cols, rows };
        let config = Config {
            scrolling_history: 10000,
            ..Default::default()
        };
        let term = term::Term::new(config, &size, NoopListener);
        Self {
            term,
            processor: Processor::new(),
            cols,
            rows,
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn process(&mut self, data: &[u8]) {
        self.processor.advance(&mut self.term, data);
    }

    #[allow(dead_code)]
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.term.resize(VTermSize { cols, rows });
    }

    /// Render current screen into a ratatui Buffer for TUI display.
    /// `scroll_offset` = lines scrolled back (0 = live view).
    pub fn render_to_buffer(
        &self,
        buf: &mut ratatui::buffer::Buffer,
        area: ratatui::layout::Rect,
        scroll_offset: usize,
        show_cursor: bool,
    ) {
        let grid = self.term.grid();
        let rows = self.rows.min(area.height);
        let cols = self.cols.min(area.width);
        let offset = scroll_offset as i32;

        for row in 0..rows {
            // With scroll_offset, shift grid line index into scrollback
            let grid_line = Line(row as i32 - offset);
            let mut col = 0u16;
            while col < cols {
                let cell = &grid[Point::new(grid_line, Column(col as usize))];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    col += 1;
                    continue;
                }
                let style = cell_to_ratatui_style(cell.fg, cell.bg, cell.flags);
                let x = area.x + col;
                let y = area.y + row;
                if x >= buf.area().x + buf.area().width || y >= buf.area().y + buf.area().height {
                    col += 1;
                    continue;
                }
                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                let buf_cell = &mut buf[(x, y)];
                buf_cell.set_char(ch).set_style(style);
                if cell.flags.contains(Flags::WIDE_CHAR) {
                    col += 2;
                } else {
                    col += 1;
                }
            }
        }

        // Render cursor only for focused pane when not scrolled back
        if show_cursor && scroll_offset == 0 {
            let cursor = grid.cursor.point;
            let cx = area.x + cursor.column.0 as u16;
            let cy = area.y + cursor.line.0 as u16;
            if cx < area.x + area.width && cy < area.y + area.height {
                let buf_cell = &mut buf[(cx, cy)];
                let style = buf_cell.style().add_modifier(ratatui::style::Modifier::REVERSED);
                buf_cell.set_style(style);
            }
        }
    }

    /// Maximum scroll offset (history size).
    pub fn max_scroll(&self) -> usize {
        use alacritty_terminal::grid::Dimensions;
        let total = self.term.grid().total_lines();
        let screen = self.term.grid().screen_lines();
        total.saturating_sub(screen)
    }

    /// Get cursor position (line, column).
    #[allow(dead_code)]
    pub fn cursor_pos(&self) -> (u16, u16) {
        let c = self.term.grid().cursor.point;
        (c.line.0 as u16, c.column.0 as u16)
    }

    /// Dump current screen as ANSI escape sequences for full redraw.
    pub fn dump_screen(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.cols as usize * self.rows as usize * 4);
        let grid = self.term.grid();
        let cols = self.cols as usize;
        let rows = self.rows as usize;

        if self.term.mode().contains(term::TermMode::ALT_SCREEN) {
            out.extend_from_slice(b"\x1b[?1049h");
        }
        out.extend_from_slice(b"\x1b[H\x1b[2J");

        let mut last_fg: Option<Color> = None;
        let mut last_bg: Option<Color> = None;
        let mut last_flags = Flags::empty();

        for line_idx in 0..rows {
            if line_idx > 0 {
                out.extend_from_slice(b"\x1b[0m\r\n");
                last_fg = None;
                last_bg = None;
                last_flags = Flags::empty();
            }

            let mut last_col = 0;
            let mut line_has_bg = false;
            for col in 0..cols {
                let cell = &grid[Point::new(Line(line_idx as i32), Column(col))];
                if cell.bg != Color::Named(NamedColor::Background) {
                    line_has_bg = true;
                }
                if cell.c != ' '
                    || !cell.flags.is_empty()
                    || cell.fg != Color::Named(NamedColor::Foreground)
                    || cell.bg != Color::Named(NamedColor::Background)
                {
                    last_col = col + 1;
                }
            }
            if line_has_bg {
                last_col = cols;
            }

            for col in 0..last_col {
                let cell = &grid[Point::new(Line(line_idx as i32), Column(col))];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                if last_fg != Some(cell.fg) || last_bg != Some(cell.bg) || last_flags != cell.flags
                {
                    out.extend_from_slice(b"\x1b[0");
                    if cell.flags.contains(Flags::BOLD) {
                        out.extend_from_slice(b";1");
                    }
                    if cell.flags.contains(Flags::DIM) {
                        out.extend_from_slice(b";2");
                    }
                    if cell.flags.contains(Flags::ITALIC) {
                        out.extend_from_slice(b";3");
                    }
                    if cell.flags.contains(Flags::UNDERLINE) {
                        out.extend_from_slice(b";4");
                    }
                    if cell.flags.contains(Flags::INVERSE) {
                        out.extend_from_slice(b";7");
                    }
                    if cell.flags.contains(Flags::STRIKEOUT) {
                        out.extend_from_slice(b";9");
                    }
                    write_color(&mut out, cell.fg, true);
                    write_color(&mut out, cell.bg, false);
                    out.push(b'm');
                    last_fg = Some(cell.fg);
                    last_bg = Some(cell.bg);
                    last_flags = cell.flags;
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
            }
        }
        out.extend_from_slice(b"\x1b[0m");
        let cursor = self.term.grid().cursor.point;
        out.extend_from_slice(
            format!("\x1b[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1).as_bytes(),
        );
        out
    }
}

/// Convert alacritty Color to ratatui Color.
fn color_to_ratatui(color: Color) -> Option<ratatui::style::Color> {
    match color {
        Color::Named(n) => {
            use ratatui::style::Color as RC;
            match n {
                NamedColor::Black | NamedColor::DimBlack => Some(RC::Black),
                NamedColor::Red | NamedColor::DimRed => Some(RC::Red),
                NamedColor::Green | NamedColor::DimGreen => Some(RC::Green),
                NamedColor::Yellow | NamedColor::DimYellow => Some(RC::Yellow),
                NamedColor::Blue | NamedColor::DimBlue => Some(RC::Blue),
                NamedColor::Magenta | NamedColor::DimMagenta => Some(RC::Magenta),
                NamedColor::Cyan | NamedColor::DimCyan => Some(RC::Cyan),
                NamedColor::White | NamedColor::DimWhite => Some(RC::White),
                NamedColor::BrightBlack => Some(RC::DarkGray),
                NamedColor::BrightRed => Some(RC::LightRed),
                NamedColor::BrightGreen => Some(RC::LightGreen),
                NamedColor::BrightYellow => Some(RC::LightYellow),
                NamedColor::BrightBlue => Some(RC::LightBlue),
                NamedColor::BrightMagenta => Some(RC::LightMagenta),
                NamedColor::BrightCyan => Some(RC::LightCyan),
                NamedColor::BrightWhite => Some(RC::White),
                NamedColor::Foreground | NamedColor::Background => None,
                _ => None,
            }
        }
        Color::Spec(rgb) => {
            // Use RGB directly — terminals that don't support true color
            // (e.g., macOS Terminal.app) will get the nearest 256-color via Indexed fallback.
            if std::env::var("COLORTERM").unwrap_or_default().contains("truecolor")
                || std::env::var("COLORTERM").unwrap_or_default().contains("24bit")
            {
                Some(ratatui::style::Color::Rgb(rgb.r, rgb.g, rgb.b))
            } else {
                // Fallback: convert RGB to nearest 256-color index
                Some(ratatui::style::Color::Indexed(rgb_to_256(rgb.r, rgb.g, rgb.b)))
            }
        }
        Color::Indexed(idx) => Some(ratatui::style::Color::Indexed(idx)),
    }
}

/// Convert RGB to the nearest 256-color index (for terminals without true color).
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    // Check grayscale ramp (232-255) first
    if r == g && g == b {
        if r < 8 {
            return 16; // black
        }
        if r > 248 {
            return 231; // white
        }
        return (((r as u16 - 8) * 24 / 247) as u8) + 232;
    }
    // Map to 6x6x6 color cube (indices 16-231)
    let ri = ((r as u16) * 5 / 255) as u8;
    let gi = ((g as u16) * 5 / 255) as u8;
    let bi = ((b as u16) * 5 / 255) as u8;
    16 + 36 * ri + 6 * gi + bi
}

/// Convert alacritty cell attributes to ratatui Style.
fn cell_to_ratatui_style(fg: Color, bg: Color, flags: Flags) -> ratatui::style::Style {
    let mut style = ratatui::style::Style::default();
    if let Some(c) = color_to_ratatui(fg) {
        style = style.fg(c);
    }
    if let Some(c) = color_to_ratatui(bg) {
        style = style.bg(c);
    }
    let mut mods = ratatui::style::Modifier::empty();
    if flags.contains(Flags::BOLD) {
        mods |= ratatui::style::Modifier::BOLD;
    }
    if flags.contains(Flags::DIM) {
        mods |= ratatui::style::Modifier::DIM;
    }
    if flags.contains(Flags::ITALIC) {
        mods |= ratatui::style::Modifier::ITALIC;
    }
    if flags.contains(Flags::UNDERLINE) {
        mods |= ratatui::style::Modifier::UNDERLINED;
    }
    if flags.contains(Flags::INVERSE) {
        mods |= ratatui::style::Modifier::REVERSED;
    }
    if flags.contains(Flags::STRIKEOUT) {
        mods |= ratatui::style::Modifier::CROSSED_OUT;
    }
    if !mods.is_empty() {
        style = style.add_modifier(mods);
    }
    style
}

fn write_color(out: &mut Vec<u8>, color: Color, is_fg: bool) {
    let base = if is_fg { 30 } else { 40 };
    match color {
        Color::Named(n) => {
            let code = match n {
                NamedColor::Black => base,
                NamedColor::Red => base + 1,
                NamedColor::Green => base + 2,
                NamedColor::Yellow => base + 3,
                NamedColor::Blue => base + 4,
                NamedColor::Magenta => base + 5,
                NamedColor::Cyan => base + 6,
                NamedColor::White => base + 7,
                NamedColor::BrightBlack => base + 60,
                NamedColor::BrightRed => base + 61,
                NamedColor::BrightGreen => base + 62,
                NamedColor::BrightYellow => base + 63,
                NamedColor::BrightBlue => base + 64,
                NamedColor::BrightMagenta => base + 65,
                NamedColor::BrightCyan => base + 66,
                NamedColor::BrightWhite => base + 67,
                NamedColor::Foreground | NamedColor::Background => return,
                NamedColor::DimBlack => base,
                NamedColor::DimRed => base + 1,
                NamedColor::DimGreen => base + 2,
                NamedColor::DimYellow => base + 3,
                NamedColor::DimBlue => base + 4,
                NamedColor::DimMagenta => base + 5,
                NamedColor::DimCyan => base + 6,
                NamedColor::DimWhite => base + 7,
                _ => return,
            };
            out.extend_from_slice(format!(";{code}").as_bytes());
        }
        Color::Spec(rgb) => {
            let p = if is_fg { 38 } else { 48 };
            out.extend_from_slice(format!(";{p};2;{};{};{}", rgb.r, rgb.g, rgb.b).as_bytes());
        }
        Color::Indexed(idx) => {
            let p = if is_fg { 38 } else { 48 };
            out.extend_from_slice(format!(";{p};5;{idx}").as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_term_with_dimensions() {
        let vt = VTerm::new(80, 24);
        assert_eq!(vt.cols, 80);
        assert_eq!(vt.rows, 24);
    }

    #[test]
    fn new_small_terminal() {
        let vt = VTerm::new(1, 1);
        assert_eq!(vt.cols, 1);
        assert_eq!(vt.rows, 1);
    }

    #[test]
    fn process_plain_text() {
        let mut vt = VTerm::new(80, 24);
        vt.process(b"Hello, world!");
        let screen = vt.dump_screen();
        let screen_str = String::from_utf8_lossy(&screen);
        assert!(screen_str.contains("Hello, world!"));
    }

    #[test]
    fn process_ansi_color() {
        let mut vt = VTerm::new(80, 24);
        vt.process(b"\x1b[31mRed\x1b[0m Normal");
        let screen = vt.dump_screen();
        let screen_str = String::from_utf8_lossy(&screen);
        assert!(screen_str.contains("Red"));
        assert!(screen_str.contains("Normal"));
    }

    #[test]
    fn process_newlines() {
        let mut vt = VTerm::new(80, 24);
        vt.process(b"line1\r\nline2");
        let screen = vt.dump_screen();
        let screen_str = String::from_utf8_lossy(&screen);
        assert!(screen_str.contains("line1"));
        assert!(screen_str.contains("line2"));
    }

    #[test]
    fn resize_updates_dimensions() {
        let mut vt = VTerm::new(80, 24);
        vt.resize(120, 40);
        assert_eq!(vt.cols, 120);
        assert_eq!(vt.rows, 40);
    }

    #[test]
    fn dump_empty_screen_has_cursor_reset() {
        let vt = VTerm::new(80, 24);
        let screen = vt.dump_screen();
        let screen_str = String::from_utf8_lossy(&screen);
        // Should have cursor home and clear screen
        assert!(screen_str.contains("\x1b[H\x1b[2J"));
        // Should end with reset + cursor position
        assert!(screen_str.contains("\x1b[0m"));
    }

    #[test]
    fn write_color_fg_red() {
        let mut out = Vec::new();
        write_color(&mut out, Color::Named(NamedColor::Red), true);
        let s = String::from_utf8_lossy(&out);
        assert_eq!(s, ";31");
    }

    #[test]
    fn write_color_bg_blue() {
        let mut out = Vec::new();
        write_color(&mut out, Color::Named(NamedColor::Blue), false);
        let s = String::from_utf8_lossy(&out);
        assert_eq!(s, ";44");
    }

    #[test]
    fn write_color_rgb() {
        let mut out = Vec::new();
        write_color(
            &mut out,
            Color::Spec(alacritty_terminal::vte::ansi::Rgb {
                r: 255,
                g: 128,
                b: 0,
            }),
            true,
        );
        let s = String::from_utf8_lossy(&out);
        assert_eq!(s, ";38;2;255;128;0");
    }

    #[test]
    fn write_color_indexed() {
        let mut out = Vec::new();
        write_color(&mut out, Color::Indexed(200), false);
        let s = String::from_utf8_lossy(&out);
        assert_eq!(s, ";48;5;200");
    }

    #[test]
    fn write_color_default_foreground_skipped() {
        let mut out = Vec::new();
        write_color(&mut out, Color::Named(NamedColor::Foreground), true);
        assert!(out.is_empty());
    }

    #[test]
    fn write_color_bright_colors() {
        let mut out = Vec::new();
        write_color(&mut out, Color::Named(NamedColor::BrightRed), true);
        let s = String::from_utf8_lossy(&out);
        assert_eq!(s, ";91");
    }

    #[test]
    fn process_then_resize_then_dump() {
        let mut vt = VTerm::new(40, 10);
        vt.process(b"Before resize");
        vt.resize(80, 24);
        vt.process(b"\r\nAfter resize");
        let screen = vt.dump_screen();
        let screen_str = String::from_utf8_lossy(&screen);
        assert!(screen_str.contains("Before resize"));
        assert!(screen_str.contains("After resize"));
    }

    #[test]
    fn process_wide_char() {
        let mut vt = VTerm::new(80, 24);
        vt.process("日本語".as_bytes());
        let screen = vt.dump_screen();
        let screen_str = String::from_utf8_lossy(&screen);
        assert!(screen_str.contains("日本語"));
    }
}
