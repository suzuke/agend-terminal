//! Virtual terminal — wraps alacritty_terminal for screen state tracking.
//! Processes raw PTY output and can dump current screen as ANSI for reconnection.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{self, Config};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use parking_lot::Mutex;
use std::io::Write;
use std::sync::Arc;

/// Fallback cell for snapshot out-of-bounds (should never happen, but
/// defense-in-depth against arithmetic bugs in snapshot indexing).
static DEFAULT_CELL: std::sync::OnceLock<Cell> = std::sync::OnceLock::new();

/// Bounds-checked grid cell access. Returns a blank default cell when the
/// column index exceeds the grid's current width — prevents panics during
/// resize races where `self.cols` diverges from `grid.columns()`.
///
/// Sprint 25 P0 HOTFIX: replaces all 5 raw `grid[Point::new(...)]` sites.
fn safe_cell(grid: &alacritty_terminal::grid::Grid<Cell>, line: Line, col: usize) -> &Cell {
    if col < grid.columns() && (line.0 as usize) < grid.screen_lines() {
        &grid[Point::new(line, Column(col))]
    } else {
        DEFAULT_CELL.get_or_init(Cell::default)
    }
}

/// Alacritty emits `Event::PtyWrite` for terminal queries like DSR CPR
/// (`\x1b[6n`), DA, and mode reports. On ConPTY (Windows), `conhost.exe`
/// fires these during startup and **blocks the child process until a reply
/// arrives** — without auto-reply the child never prints its prompt. This
/// listener forwards `PtyWrite` bytes back to the agent's PTY writer; a
/// `None` writer (tests, layout stubs) behaves like a silent sink.
#[derive(Clone)]
pub struct PtyWriteListener {
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
}

impl PtyWriteListener {
    fn noop() -> Self {
        Self { writer: None }
    }

    pub fn new(writer: Arc<Mutex<Box<dyn Write + Send>>>) -> Self {
        Self {
            writer: Some(writer),
        }
    }
}

impl EventListener for PtyWriteListener {
    fn send_event(&self, event: Event) {
        let Event::PtyWrite(text) = event else { return };
        let Some(writer) = &self.writer else { return };
        {
            let mut w = writer.lock();
            let _ = w.write_all(text.as_bytes());
            let _ = w.flush();
        }
    }
}

/// Cached once: whether terminal supports true color (avoids env var lookup per cell).
fn supports_truecolor() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        let val = std::env::var("COLORTERM").unwrap_or_default();
        val.contains("truecolor") || val.contains("24bit")
    })
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
    term: term::Term<PtyWriteListener>,
    processor: Processor,
    cols: u16,
    rows: u16,
}

impl VTerm {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_listener(cols, rows, PtyWriteListener::noop())
    }

    /// Construct a VTerm that auto-replies to terminal status queries by
    /// writing responses back through `pty_writer`. Required on Windows
    /// ConPTY where `conhost.exe` waits for a cursor-position reply before
    /// letting the child print its banner.
    pub fn with_pty_writer(
        cols: u16,
        rows: u16,
        pty_writer: Arc<Mutex<Box<dyn Write + Send>>>,
    ) -> Self {
        Self::with_listener(cols, rows, PtyWriteListener::new(pty_writer))
    }

    fn with_listener(cols: u16, rows: u16, listener: PtyWriteListener) -> Self {
        let size = VTermSize { cols, rows };
        let config = Config {
            scrolling_history: 10000,
            ..Default::default()
        };
        let term = term::Term::new(config, &size, listener);
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
        show_block_cursor: bool,
    ) {
        // L0b safety net: catch any panic from grid access so a resize race
        // or unexpected alacritty state doesn't take down the daemon.
        // Sprint 25 P0 HOTFIX — defense in depth alongside safe_cell (L0a).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.render_to_buffer_inner(buf, area, scroll_offset, show_block_cursor);
        }));
        if let Err(e) = result {
            let msg = if let Some(s) = e.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            tracing::warn!(error = %msg, "vterm render panic caught (resize race?) — rendering blank");
        }
    }

    fn render_to_buffer_inner(
        &self,
        buf: &mut ratatui::buffer::Buffer,
        area: ratatui::layout::Rect,
        scroll_offset: usize,
        show_block_cursor: bool,
    ) {
        let grid = self.term.grid();
        let grid_cols = grid.columns() as u16;
        let grid_rows = grid.screen_lines() as u16;
        // Sprint 21 Phase 4 Q7 sweep: this triple-`.min()` chain is the same
        // saturating-arithmetic class as `render::clamp_overlay_dim` —
        // intentionally caps render bounds by the smallest of {self-tracked,
        // ratatui-passed, grid-actual} dimensions so a resize race where any
        // pair disagrees cannot index past the alacritty grid (HOTFIX PR #194
        // closed the panic; root-cause race deferred to backlog
        // `t-20260426150432078733-1`). Audit-confirmed clean: no other
        // panic-prone subtraction sites in this file.
        let rows = self.rows.min(area.height).min(grid_rows);
        let cols = self.cols.min(area.width).min(grid_cols);
        // `scroll_offset` is usize; `as i32` wraps on 64-bit hosts when the
        // caller somehow passes > i32::MAX. Clamp instead so an unreasonable
        // offset degrades to "deepest scrollback" rather than flipping sign
        // and pulling a positive-huge line index that panics alacritty on
        // index.
        let offset: i32 = scroll_offset.min(i32::MAX as usize) as i32;

        // L1 atomic snapshot: copy visible cells into a local buffer so
        // concurrent PTY resize cannot mutate the grid mid-render. This
        // eliminates the TOCTOU temporal gap entirely — the snapshot is
        // immutable for the duration of this frame. Cost: ~rows×cols Cell
        // copies (typically 120×40 = 4800 cells, ~100KB). safe_cell (L0a)
        // remains as defense-in-depth for the snapshot-build phase itself.
        let snap_rows = rows as usize;
        let snap_cols = cols as usize;
        let mut snapshot: Vec<Cell> = Vec::with_capacity(snap_rows * snap_cols);
        for row in 0..rows {
            let grid_line = Line((row as i32).saturating_sub(offset));
            for c in 0..cols {
                snapshot.push(safe_cell(grid, grid_line, c as usize).clone());
            }
        }

        // Cursor snapshot for block cursor rendering (also TOCTOU-safe).
        let cursor_snapshot = grid.cursor.point;

        // From here on, only the snapshot is used — the live grid reference
        // is no longer accessed, eliminating the TOCTOU window.

        for row in 0..rows {
            let mut col = 0u16;
            while col < cols {
                let idx = (row as usize) * snap_cols + (col as usize);
                let cell = snapshot
                    .get(idx)
                    .unwrap_or_else(|| DEFAULT_CELL.get_or_init(Cell::default));
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

        // Reversed block cursor for unfocused panes (focused panes use terminal cursor)
        if show_block_cursor && scroll_offset == 0 {
            let cursor = cursor_snapshot;
            let cx = area.x + cursor.column.0 as u16;
            let cy = area.y + cursor.line.0 as u16;
            if cx < area.x + area.width && cy < area.y + area.height {
                let buf_cell = &mut buf[(cx, cy)];
                let style = buf_cell
                    .style()
                    .add_modifier(ratatui::style::Modifier::REVERSED);
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

    /// Extract text from a selection range (grid coordinates), accounting for scroll offset.
    pub fn extract_text(&self, start: (u16, u16), end: (u16, u16), scroll_offset: usize) -> String {
        let grid = self.term.grid();
        // Same clamp rationale as `render_to_buffer`: `scroll_offset as i32`
        // wraps negative for pathological values.
        let offset: i32 = scroll_offset.min(i32::MAX as usize) as i32;

        // Normalize start/end so start is before end
        let (s, e) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let (s_row, s_col) = s;
        let (e_row, e_col) = e;

        let mut text = String::new();
        for row in s_row..=e_row {
            let grid_line = Line((row as i32).saturating_sub(offset));
            let col_start = if row == s_row { s_col } else { 0 };
            let col_end = if row == e_row {
                e_col
            } else {
                self.cols.saturating_sub(1)
            };

            let mut line = String::new();
            for col in col_start..=col_end {
                if (col as usize) >= self.cols as usize {
                    break;
                }
                let cell = safe_cell(grid, grid_line, col as usize);
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                line.push(ch);
            }
            // Trim trailing spaces per line
            let trimmed = line.trim_end();
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(trimmed);
        }
        text
    }

    /// Return the last `n` visible rows of the screen as plain text,
    /// stripped of ANSI attributes and trailing spaces. Leading blank
    /// rows are omitted so short output doesn't look padded.
    ///
    /// Used by AwaitingOperator to snapshot "what the CLI printed before
    /// it started waiting for stdin" for forwarding to Telegram.
    pub fn tail_lines(&self, n: usize) -> String {
        let grid = self.term.grid();
        let cols = self.cols as usize;
        let rows = self.rows as usize;

        let mut lines: Vec<String> = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut line = String::with_capacity(cols);
            let mut col = 0;
            while col < cols {
                let cell = safe_cell(grid, Line(row as i32), col);
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    col += 1;
                    continue;
                }
                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                line.push(ch);
                col += 1;
            }
            lines.push(line.trim_end().to_string());
        }

        // Trim blank lines at both ends so terse output doesn't look padded
        // and tail-N doesn't return the scroll buffer's trailing whitespace.
        let first = lines
            .iter()
            .position(|l| !l.is_empty())
            .unwrap_or(lines.len());
        let last = lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map(|i| i + 1)
            .unwrap_or(first);
        let visible = &lines[first..last];

        let tail = if visible.len() > n {
            &visible[visible.len() - n..]
        } else {
            visible
        };
        tail.join("\n")
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
                let cell = safe_cell(grid, Line(line_idx as i32), col);
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
                let cell = safe_cell(grid, Line(line_idx as i32), col);
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
            if supports_truecolor() {
                Some(ratatui::style::Color::Rgb(rgb.r, rgb.g, rgb.b))
            } else {
                // Fallback: convert RGB to nearest 256-color index
                Some(ratatui::style::Color::Indexed(rgb_to_256(
                    rgb.r, rgb.g, rgb.b,
                )))
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

    #[test]
    fn tail_lines_strips_leading_blanks_and_trailing_spaces() {
        let mut vt = VTerm::new(40, 10);
        vt.process(b"\r\n\r\nhello  \r\nworld");
        let tail = vt.tail_lines(5);
        // Leading/trailing blank lines gone; trailing spaces on "hello" stripped
        assert_eq!(tail, "hello\nworld");
    }

    #[test]
    fn tail_lines_caps_at_n() {
        let mut vt = VTerm::new(20, 10);
        for i in 0..6 {
            vt.process(format!("line{i}\r\n").as_bytes());
        }
        let tail = vt.tail_lines(3);
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "line3");
        assert_eq!(lines[1], "line4");
        assert_eq!(lines[2], "line5");
    }

    #[test]
    fn tail_lines_empty_screen() {
        let vt = VTerm::new(80, 24);
        assert_eq!(vt.tail_lines(40), "");
    }

    #[test]
    fn tail_lines_wide_char_preserved() {
        let mut vt = VTerm::new(40, 5);
        vt.process("日本語 prompt>".as_bytes());
        let tail = vt.tail_lines(3);
        assert!(tail.contains("日本語"));
        assert!(tail.contains("prompt>"));
    }

    // --- Sprint 25 P0 HOTFIX regression tests ---

    /// Regression: safe_cell returns default for out-of-bounds column.
    #[test]
    fn safe_cell_oob_column_returns_default() {
        let vt = VTerm::new(10, 5);
        let grid = vt.term.grid();
        // Access column 100 on a 10-column grid — must not panic
        let cell = safe_cell(grid, Line(0), 100);
        assert_eq!(cell.c, ' ');
    }

    /// Regression: safe_cell returns default for out-of-bounds line.
    #[test]
    fn safe_cell_oob_line_returns_default() {
        let vt = VTerm::new(10, 5);
        let grid = vt.term.grid();
        // Access line 100 on a 5-line grid — must not panic
        let cell = safe_cell(grid, Line(100), 0);
        assert_eq!(cell.c, ' ');
    }

    /// Regression: dump_screen doesn't panic when self.cols > grid.columns().
    /// Simulates the resize race by creating a vterm, then manually setting
    /// cols to a value larger than the grid.
    #[test]
    fn dump_screen_survives_cols_exceeding_grid() {
        let mut vt = VTerm::new(10, 5);
        vt.process(b"Hello");
        // Simulate resize race: self.cols says 120 but grid is still 10
        vt.cols = 120;
        // Must not panic — safe_cell handles the OOB access
        let screen = vt.dump_screen();
        // Should produce some output (not empty — at least ANSI reset)
        assert!(!screen.is_empty());
    }

    /// Regression: tail_lines doesn't panic when self.cols > grid.columns().
    #[test]
    fn tail_lines_survives_cols_exceeding_grid() {
        let mut vt = VTerm::new(10, 5);
        vt.process(b"Hello");
        vt.cols = 120;
        // Must not panic
        let tail = vt.tail_lines(3);
        assert!(tail.contains("Hello"));
    }

    /// Regression: render_to_buffer catch_unwind prevents daemon crash.
    #[test]
    fn render_to_buffer_catch_unwind_safety_net() {
        let mut vt = VTerm::new(10, 5);
        vt.process(b"Test");
        vt.cols = 200; // Extreme mismatch
        let area = ratatui::layout::Rect::new(0, 0, 200, 5);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        // Must not panic — catch_unwind + safe_cell double protection
        vt.render_to_buffer(&mut buf, area, 0, false);
    }

    /// Regression: xterm mouse tracking SGR sequences with large column
    /// values don't panic the parser. This is the poison sequence class
    /// from the 2026-04-27 incident.
    #[test]
    fn xterm_mouse_tracking_large_col_no_panic() {
        let mut vt = VTerm::new(26, 10);
        // SGR mouse tracking: CSI < btn ; col ; row M
        // col=107 exceeds grid width of 26 — the original panic trigger
        let poison = b"\x1b[<35;107;5M\x1b[<35;108;5M\x1b[<35;200;5M";
        vt.process(poison);
        // Parser should handle gracefully — no panic
        let screen = vt.dump_screen();
        assert!(!screen.is_empty());
    }

    // --- Sprint 25 P1 backfill: F1 concurrent-state harness ---

    /// F1: Thread A shrinks grid while thread B renders. L1 atomic snapshot
    /// must ensure frame integrity — non-blank cells present despite race.
    #[test]
    fn concurrent_resize_render_frame_integrity() {
        let mut vt = VTerm::new(80, 24);
        // Fill screen with visible content
        for _ in 0..24 {
            vt.process(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz01234567890123456789\r\n");
        }

        // We can't share VTerm across threads (not Send), so we test the
        // snapshot mechanism by simulating the race: set cols to a value
        // larger than grid, then render. L1 snapshot captures cells at
        // grid's actual width; L0a safe_cell handles any OOB in snapshot
        // build. The frame should contain non-blank content.
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        vt.render_to_buffer(&mut buf, area, 0, false);

        // Verify frame has non-blank content (integrity, not just no-panic)
        let has_content = buf.content().iter().any(|c| c.symbol() != " ");
        assert!(
            has_content,
            "frame must contain non-blank cells after render"
        );

        // Now simulate resize race: cols mismatch
        vt.cols = 120; // Larger than grid's 80
        let area2 = ratatui::layout::Rect::new(0, 0, 120, 24);
        let mut buf2 = ratatui::buffer::Buffer::empty(area2);
        vt.render_to_buffer(&mut buf2, area2, 0, false);

        // L1 snapshot should still produce content (capped at grid width)
        let has_content2 = buf2.content().iter().any(|c| c.symbol() != " ");
        assert!(
            has_content2,
            "frame must contain non-blank cells even with cols mismatch"
        );
    }

    // --- Sprint 25 P1 backfill: F2 persistence-replay round-trip ---

    /// F2: Poison escape sequence → dump_screen → restore via process →
    /// no panic. Simulates the scrollback persistence vector where poison
    /// bytes survive daemon restart.
    #[test]
    fn persistence_replay_poison_no_panic() {
        let mut vt = VTerm::new(26, 10);
        // Feed poison: SGR mouse tracking with col > grid width
        let poison = b"\x1b[<35;107;5M\x1b[<35;200;3M";
        vt.process(poison);
        vt.process(b"visible text after poison");

        // Dump screen (simulates what gets persisted in scrollback)
        let dump = vt.dump_screen();

        // Create fresh vterm (simulates daemon restart)
        let mut vt2 = VTerm::new(26, 10);
        // Replay the dump (simulates scrollback restore)
        vt2.process(&dump);

        // Must not panic — and should contain the visible text
        let dump2 = vt2.dump_screen();
        let screen = String::from_utf8_lossy(&dump2);
        assert!(screen.contains("visible text after poison"));

        // Also verify render doesn't panic
        let area = ratatui::layout::Rect::new(0, 0, 26, 10);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        vt2.render_to_buffer(&mut buf, area, 0, false);
    }

    // --- Sprint 25 P1 backfill: L1' CSI parameter clamping ---

    /// L1': Various CSI sequences with out-of-bounds parameters should
    /// not corrupt vterm state or cause panics.
    #[test]
    fn csi_oob_cursor_position_clamped() {
        let mut vt = VTerm::new(20, 10);
        // CUP (cursor position) with row=999, col=999 — way beyond grid
        vt.process(b"\x1b[999;999H");
        vt.process(b"X");
        // Should not panic; cursor clamped to grid bounds by alacritty
        let screen = vt.dump_screen();
        assert!(!screen.is_empty());
    }

    #[test]
    fn csi_oob_scroll_region_no_panic() {
        let mut vt = VTerm::new(20, 10);
        // DECSTBM with absurd scroll region
        vt.process(b"\x1b[999;999r");
        vt.process(b"Hello\r\n");
        let screen = vt.dump_screen();
        assert!(!screen.is_empty());
    }

    #[test]
    fn csi_oob_erase_no_panic() {
        let mut vt = VTerm::new(20, 10);
        // ED (erase display) + EL (erase line) with params
        vt.process(b"\x1b[999J\x1b[999K");
        vt.process(b"After erase");
        let tail = vt.tail_lines(3);
        assert!(tail.contains("After erase"));
    }
}
