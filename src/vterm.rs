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
            scrolling_history: 0,
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

    pub fn process(&mut self, data: &[u8]) {
        self.processor.advance(&mut self.term, data);
    }

    #[allow(dead_code)]
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.term.resize(VTermSize { cols, rows });
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
