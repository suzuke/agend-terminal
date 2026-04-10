use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{self, Config};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

/// Minimal event listener — we don't render, just track state.
#[derive(Clone)]
struct NoopListener;

impl EventListener for NoopListener {
    fn send_event(&self, _event: Event) {}
}

/// Virtual terminal size.
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

/// Virtual terminal that processes PTY output and maintains screen state.
pub struct VTerm {
    term: term::Term<NoopListener>,
    processor: Processor,
    cols: u16,
    rows: u16,
}

impl VTerm {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = VTermSize { cols, rows };
        let mut config = Config::default();
        config.scrolling_history = 0; // No scrollback — only visible screen needed
        let term = term::Term::new(config, &size, NoopListener);
        Self {
            term,
            processor: Processor::new(),
            cols,
            rows,
        }
    }

    /// Feed raw PTY output bytes into the virtual terminal.
    pub fn process(&mut self, data: &[u8]) {
        self.processor.advance(&mut self.term, data);
    }

    /// Resize the virtual terminal.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        let size = VTermSize { cols, rows };
        self.term.resize(size);
    }

    /// Dump current screen content as ANSI escape sequences for full redraw.
    pub fn dump_screen(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.cols as usize * self.rows as usize * 4);
        let grid = self.term.grid();
        let cols = self.cols as usize;
        let rows = self.rows as usize;

        // Enter alternate screen if the term is in it
        if self.term.mode().contains(term::TermMode::ALT_SCREEN) {
            out.extend_from_slice(b"\x1b[?1049h");
        }

        // Home cursor + clear screen
        out.extend_from_slice(b"\x1b[H\x1b[2J");

        let mut last_fg: Option<Color> = None;
        let mut last_bg: Option<Color> = None;
        let mut last_flags = Flags::empty();

        for line_idx in 0..rows {
            if line_idx > 0 {
                // Reset before newline to prevent bg color bleeding into margins
                out.extend_from_slice(b"\x1b[0m\r\n");
                last_fg = None;
                last_bg = None;
                last_flags = Flags::empty();
            }

            // Find last non-empty column.
            // If any cell on this line has a non-default bg (e.g., statusline),
            // emit the full line width to preserve bg color layout.
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

                // Skip wide char spacer cells — the wide char already occupies 2 columns
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                // Emit SGR if attributes changed
                let need_sgr = last_fg != Some(cell.fg)
                    || last_bg != Some(cell.bg)
                    || last_flags != cell.flags;

                if need_sgr {
                    out.extend_from_slice(b"\x1b[0"); // Reset first

                    // Flags
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

                    // Foreground
                    write_color(&mut out, cell.fg, true);
                    // Background
                    write_color(&mut out, cell.bg, false);

                    out.push(b'm');

                    last_fg = Some(cell.fg);
                    last_bg = Some(cell.bg);
                    last_flags = cell.flags;
                }

                // Write character
                let mut buf = [0u8; 4];
                out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
            }
        }

        // Reset attributes
        out.extend_from_slice(b"\x1b[0m");

        // Position cursor
        let cursor = self.term.grid().cursor.point;
        let cursor_line = cursor.line.0 + 1; // 1-based
        let cursor_col = cursor.column.0 + 1; // 1-based
        out.extend_from_slice(format!("\x1b[{cursor_line};{cursor_col}H").as_bytes());

        out
    }
}

fn write_color(out: &mut Vec<u8>, color: Color, is_fg: bool) {
    let base = if is_fg { 30 } else { 40 };
    match color {
        Color::Named(named) => {
            let code = match named {
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
                // Default foreground/background — don't emit
                NamedColor::Foreground | NamedColor::Background => return,
                // Dim variants map to same as normal (with DIM flag)
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
            let prefix = if is_fg { 38 } else { 48 };
            out.extend_from_slice(
                format!(";{prefix};2;{};{};{}", rgb.r, rgb.g, rgb.b).as_bytes(),
            );
        }
        Color::Indexed(idx) => {
            let prefix = if is_fg { 38 } else { 48 };
            out.extend_from_slice(format!(";{prefix};5;{idx}").as_bytes());
        }
    }
}
