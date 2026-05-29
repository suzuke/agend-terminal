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
    use alacritty_terminal::grid::Dimensions;
    if col < grid.columns() && line >= grid.topmost_line() && line <= grid.bottommost_line() {
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
        // #1064 area-clamp pre-fill: ratatui Buffer is stateful across
        // frames (codebase invariant from #819). The clamped-write loop
        // below only touches `rows × cols = min(self.rows, area.height,
        // grid_rows)` cells, so trailing rows/cols of `area` (when
        // `area > grid`, e.g. resize race or initial spawn lag) would
        // retain previous-frame content → operator-visible residual text.
        // Blank the full area first with a default cell; the loop then
        // overwrites with PTY content for the clamped region.
        let buf_right = buf.area().x.saturating_add(buf.area().width);
        let buf_bottom = buf.area().y.saturating_add(buf.area().height);
        let default_style = ratatui::style::Style::default();
        for dy in 0..area.height {
            let y = area.y.saturating_add(dy);
            if y >= buf_bottom {
                break;
            }
            for dx in 0..area.width {
                let x = area.x.saturating_add(dx);
                if x >= buf_right {
                    break;
                }
                let cell = &mut buf[(x, y)];
                cell.set_char(' ');
                cell.set_style(default_style);
            }
        }

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
                    // Defensive: the WIDE_CHAR branch below skips col+1
                    // by advancing `col += 2`, so this arm is normally
                    // unreachable when iterating from a WIDE_CHAR's
                    // sibling position. Kept as a guard for
                    // partial-render edge cases (e.g. col 0 starts on
                    // a SPACER because the WIDE_CHAR fell off the left
                    // edge during a resize).
                    let x = area.x + col;
                    let y = area.y + row;
                    if x < buf.area().x + buf.area().width && y < buf.area().y + buf.area().height {
                        let style = cell_to_ratatui_style(cell.fg, cell.bg, cell.flags);
                        let buf_cell = &mut buf[(x, y)];
                        buf_cell.set_char(' ').set_style(style);
                    }
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
                    // #819: when writing a wide char at (x, y), the
                    // adjacent cell at (x+1, y) is the WIDE_CHAR_SPACER
                    // position. The outer loop `col += 2` skips it
                    // entirely, so without an explicit blank-write here
                    // the staging buffer retains the previous frame's
                    // char at (x+1, y) — root cause of the "scattered
                    // chars" operator observation (fixup-lead's CJK
                    // prompt). Mirror the style from the wide char so
                    // background continuity is preserved.
                    let spacer_x = x + 1;
                    if spacer_x < buf.area().x + buf.area().width
                        && y < buf.area().y + buf.area().height
                    {
                        let spacer_cell = &mut buf[(spacer_x, y)];
                        spacer_cell.set_char(' ').set_style(style);
                    }
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

    /// Returns true if the terminal application has enabled mouse reporting.
    /// Uses `intersects` because alacritty_terminal stores the three mouse-mode
    /// bits (1000/1002/1003) as mutually-exclusive (each `[?1xxxh]` removes the
    /// whole MOUSE_MODE set first, then inserts one bit). `contains` would
    /// require all three bits and therefore never fire.
    pub fn wants_mouse(&self) -> bool {
        self.term.mode().intersects(term::TermMode::MOUSE_MODE)
    }

    /// Returns true if SGR mouse encoding is active (CSI < format).
    pub fn mouse_sgr(&self) -> bool {
        self.term.mode().contains(term::TermMode::SGR_MOUSE)
    }

    /// Maximum scroll offset (history size).
    pub fn max_scroll(&self) -> usize {
        use alacritty_terminal::grid::Dimensions;
        let total = self.term.grid().total_lines();
        let screen = self.term.grid().screen_lines();
        total.saturating_sub(screen)
    }

    /// Get cursor position (line, column).
    pub fn cursor_pos(&self) -> (u16, u16) {
        let c = self.term.grid().cursor.point;
        (c.line.0 as u16, c.column.0 as u16)
    }

    /// Extract text from a selection range given in absolute scrollback logical
    /// coordinates (`.0` = `grid_line + max_scroll()`, `.1` = column).
    ///
    /// Offset-independent: the anchor is captured once and resolves to the same
    /// content regardless of later scrolling or new output. Lines that have
    /// scrolled past the history cap resolve to blanks via `safe_cell`.
    pub fn extract_text(&self, start: (i64, u16), end: (i64, u16)) -> String {
        let max_scroll = self.max_scroll() as i64;
        let grid = self.term.grid();

        // Normalize start/end so start is before end by (logical line, col).
        let (s, e) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let (s_line, s_col) = s;
        let (e_line, e_col) = e;

        let mut text = String::new();
        for logical in s_line..=e_line {
            // logical → grid line: the oldest buffer line is at -max_scroll.
            let grid_line =
                Line((logical - max_scroll).clamp(i32::MIN as i64, i32::MAX as i64) as i32);
            let col_start = if logical == s_line { s_col } else { 0 };
            let col_end = if logical == e_line {
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

    /// Read scrollback history + visible screen as plain text (ANSI stripped).
    /// Returns the last `max_lines` lines. Walks from `topmost_line()` to
    /// `bottommost_line()` via `safe_cell` — same pattern as `tail_lines`.
    pub fn read_scrollback(&self, max_lines: usize) -> String {
        let grid = self.term.grid();
        let cols = grid.columns();
        let top = grid.topmost_line();
        let bot = grid.bottommost_line();

        // Read ALL lines first (trim blanks before windowing so content
        // above trailing blank padding is not lost — gemini-banner case).
        let mut lines: Vec<String> = Vec::new();
        let mut row = top;
        while row <= bot {
            let mut line = String::with_capacity(cols);
            let mut col = 0;
            while col < cols {
                let cell = safe_cell(grid, row, col);
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    col += 1;
                    continue;
                }
                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                line.push(ch);
                col += 1;
            }
            lines.push(line.trim_end().to_string());
            row += 1;
        }

        // Trim blank lines at both ends BEFORE windowing
        let first = lines
            .iter()
            .position(|l| !l.is_empty())
            .unwrap_or(lines.len());
        let last = lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map(|i| i + 1)
            .unwrap_or(first);
        let trimmed = &lines[first..last];

        // Window to last max_lines
        let result = if trimmed.len() > max_lines {
            &trimmed[trimmed.len() - max_lines..]
        } else {
            trimmed
        };
        result.join("\n")
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

    #[test]
    fn ed2_clears_all_residual_text() {
        let mut vt = VTerm::new(40, 10);
        vt.process(b"RESIDUAL LINE ONE\r\n");
        vt.process(b"RESIDUAL LINE TWO\r\n");
        vt.process(b"RESIDUAL LINE THREE");
        let before = vt.tail_lines(10);
        assert!(before.contains("RESIDUAL"), "precondition: text present");
        // ED 2 = erase entire display
        vt.process(b"\x1b[2J\x1b[H");
        let after = vt.tail_lines(10);
        assert!(
            !after.contains("RESIDUAL"),
            "ED 2 must clear all residual text, got: {after:?}"
        );
    }

    #[test]
    fn el2_clears_entire_line() {
        let mut vt = VTerm::new(40, 5);
        vt.process(b"HELLO WORLD");
        // Move cursor to col 5, then EL 2 = erase entire line
        vt.process(b"\x1b[1;6H\x1b[2K");
        let after = vt.tail_lines(5);
        assert!(
            !after.contains("HELLO"),
            "EL 2 must clear entire line, got: {after:?}"
        );
    }

    #[test]
    fn el0_clears_from_cursor_to_eol() {
        let mut vt = VTerm::new(40, 5);
        vt.process(b"ABCDEFGHIJ");
        // Move cursor to col 5 (1-indexed), EL 0 = erase cursor to end of line
        vt.process(b"\x1b[1;6H\x1b[0K");
        let after = vt.tail_lines(5);
        assert!(
            after.contains("ABCDE"),
            "text before cursor preserved, got: {after:?}"
        );
        assert!(
            !after.contains("FGHIJ"),
            "text after cursor cleared, got: {after:?}"
        );
    }

    #[test]
    fn ed0_clears_from_cursor_to_end_of_display() {
        let mut vt = VTerm::new(40, 5);
        vt.process(b"LINE1\r\nLINE2\r\nLINE3");
        // Move to row 2 col 1, ED 0 = erase from cursor to end of display
        vt.process(b"\x1b[2;1H\x1b[0J");
        let after = vt.tail_lines(5);
        assert!(after.contains("LINE1"), "line1 preserved, got: {after:?}");
        assert!(!after.contains("LINE2"), "line2 cleared, got: {after:?}");
        assert!(!after.contains("LINE3"), "line3 cleared, got: {after:?}");
    }

    #[test]
    fn erase_display_scroll_region_clears_within_region() {
        let mut vt = VTerm::new(40, 10);
        // Set scroll region to rows 3-7 (1-indexed)
        vt.process(b"\x1b[3;7r");
        // Fill some text
        vt.process(b"\x1b[1;1HTOP\r\n");
        vt.process(b"\x1b[3;1HMIDDLE\r\n");
        vt.process(b"\x1b[8;1HBOTTOM");
        // Erase display within scroll region
        vt.process(b"\x1b[3;1H\x1b[J");
        let after = vt.tail_lines(10);
        assert!(
            after.contains("TOP"),
            "text outside scroll region preserved, got: {after:?}"
        );
        assert!(
            !after.contains("MIDDLE"),
            "text inside scroll region cleared, got: {after:?}"
        );
    }

    #[test]
    fn ech_erases_n_characters_at_cursor() {
        let mut vt = VTerm::new(40, 5);
        vt.process(b"ABCDEFGHIJ");
        // Move to col 3 (1-indexed), ECH 4 = erase 4 chars at cursor
        vt.process(b"\x1b[1;4H\x1b[4X");
        let after = vt.tail_lines(5);
        assert!(
            after.contains("ABC"),
            "text before cursor preserved, got: {after:?}"
        );
        assert!(
            !after.contains("DEFG"),
            "4 chars at cursor erased, got: {after:?}"
        );
        assert!(
            after.contains("HIJ"),
            "text after erased range preserved, got: {after:?}"
        );
    }

    #[test]
    fn dump_screen_after_ed2_has_no_residual() {
        let mut vt = VTerm::new(40, 10);
        vt.process(b"RESIDUAL TEXT LINE 1\r\n");
        vt.process(b"RESIDUAL TEXT LINE 2\r\n");
        vt.process(b"RESIDUAL TEXT LINE 3");
        // ED 2 + cursor home
        vt.process(b"\x1b[2J\x1b[H");
        vt.process(b"FRESH CONTENT");
        let raw = vt.dump_screen();
        let dump = String::from_utf8_lossy(&raw);
        assert!(
            !dump.contains("RESIDUAL"),
            "dump_screen must not contain erased text, got residual in dump"
        );
        assert!(dump.contains("FRESH"), "dump_screen must contain new text");
    }

    #[test]
    fn alt_screen_switch_no_residual() {
        let mut vt = VTerm::new(40, 10);
        vt.process(b"MAIN SCREEN TEXT");
        // Enter alt screen
        vt.process(b"\x1b[?1049h");
        vt.process(b"ALT SCREEN TEXT");
        let alt = vt.tail_lines(10);
        assert!(alt.contains("ALT SCREEN"), "alt screen shows alt text");
        assert!(!alt.contains("MAIN SCREEN"), "alt screen hides main text");
        // Exit alt screen
        vt.process(b"\x1b[?1049l");
        let main = vt.tail_lines(10);
        assert!(main.contains("MAIN SCREEN"), "main screen restored");
        assert!(!main.contains("ALT SCREEN"), "alt screen text gone");
    }

    #[test]
    fn read_scrollback_returns_visible_and_history() {
        let mut vt = VTerm::new(80, 5);
        // Write 10 lines into a 5-row terminal — first 5 scroll into history
        for i in 1..=10 {
            vt.process(format!("line{i}\r\n").as_bytes());
        }
        let text = vt.read_scrollback(100);
        assert!(
            text.contains("line1"),
            "scrollback must include history line1, got: {text}"
        );
        assert!(
            text.contains("line10"),
            "scrollback must include visible line10, got: {text}"
        );
    }

    #[test]
    fn read_scrollback_limits_to_n_lines() {
        let mut vt = VTerm::new(80, 5);
        for i in 1..=20 {
            vt.process(format!("line{i}\r\n").as_bytes());
        }
        let text = vt.read_scrollback(3);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines.len() <= 3,
            "read_scrollback(3) must return at most 3 lines, got {}",
            lines.len()
        );
    }

    #[test]
    fn read_scrollback_empty_terminal() {
        let vt = VTerm::new(80, 24);
        let text = vt.read_scrollback(100);
        assert!(text.is_empty(), "empty terminal must return empty string");
    }

    #[test]
    fn read_scrollback_trims_leading_blanks_then_windows() {
        // Gemini-banner case: content at top, then 120+ blank padding rows.
        // With a 50-line window, the old code captures the last 50 rows
        // (all blank) and returns empty despite real content above.
        let mut vt = VTerm::new(80, 10);
        // Content first
        for i in 1..=5 {
            vt.process(format!("TESTLINE{i}\r\n").as_bytes());
        }
        // Then push 120 blank lines (simulates gemini padding)
        for _ in 0..120 {
            vt.process(b"\r\n");
        }
        let text = vt.read_scrollback(50);
        assert!(
            text.contains("TESTLINE1"),
            "content above blank padding must surface, got: '{text}'"
        );
    }

    #[test]
    fn read_scrollback_empty_pty_still_returns_empty() {
        // Regression guard: empty PTY must still return empty string
        let vt = VTerm::new(80, 24);
        let text = vt.read_scrollback(100);
        assert!(text.is_empty(), "empty PTY must return empty string");
    }

    // #700 regression guard: alacritty_terminal stores mouse-mode bits
    // (1000/1002/1003) as mutually-exclusive. `contains(MOUSE_MODE)` requires
    // all three bits and therefore never fires for real backends, which is
    // exactly the bug that let mouse forwarding silently skip every event.
    #[test]
    fn wants_mouse_detects_single_mouse_mode_bit() {
        for seq in [
            b"\x1b[?1000h".as_slice(), // click reporting
            b"\x1b[?1002h",            // button-event tracking
            b"\x1b[?1003h",            // any-motion tracking
        ] {
            let mut vt = VTerm::new(80, 24);
            vt.process(seq);
            assert!(
                vt.wants_mouse(),
                "wants_mouse must be true after {:?}",
                String::from_utf8_lossy(seq)
            );
        }
    }

    #[test]
    fn wants_mouse_matches_opencode_startup_sequence() {
        // Exact sequence from tests/fixtures/state-replay/opencode-thinking.raw
        let mut vt = VTerm::new(80, 24);
        vt.process(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h");
        assert!(
            vt.wants_mouse(),
            "wants_mouse must be true after opencode startup"
        );
        assert!(vt.mouse_sgr(), "mouse_sgr must be true after 1006h");
    }

    #[test]
    fn wants_mouse_false_without_mouse_mode() {
        let mut vt = VTerm::new(80, 24);
        vt.process(b"plain text \x1b[31mred\x1b[0m");
        assert!(!vt.wants_mouse());
        assert!(!vt.mouse_sgr());
    }

    // ── #819 WIDE_CHAR_SPACER stale-char bug ──

    #[test]
    fn test_wide_char_spacer_clears_stale_buf_cell() {
        // #819 RED test: at Site 1 (`render_to_buffer_inner`), the
        // WIDE_CHAR_SPACER `continue` skips writing buf[(x, y)]. The
        // ratatui Buffer is stateful across frames — previous frame's
        // content at the spacer position survives. This test
        // pre-poisons the spacer cell with a sentinel char, then
        // renders. Pre-fix: sentinel survives (BUG). Post-fix:
        // sentinel replaced with blank (SUT writes the spacer cell).
        let mut vt = VTerm::new(10, 1);
        // A CJK char has display width 2 — alacritty marks col 0 as
        // WIDE_CHAR and col 1 as WIDE_CHAR_SPACER.
        vt.process("中".as_bytes());
        let area = ratatui::layout::Rect::new(0, 0, 10, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        // Pre-poison col=1 (the SPACER position) with a stale sentinel
        // simulating a previous frame's char at that position.
        buf[(1, 0)].set_char('X');
        // Render the current frame.
        vt.render_to_buffer(&mut buf, area, 0, false);
        // SPACER position must be blanked, NOT carry the previous
        // frame's 'X'. This is the #819 fix's invariant.
        assert_eq!(
            buf[(1, 0)].symbol(),
            " ",
            "WIDE_CHAR_SPACER position must be blanked, got: {:?}",
            buf[(1, 0)].symbol()
        );
    }

    #[test]
    fn test_wide_char_spacer_preserves_wide_char_at_col_minus_one() {
        // #819 adjacency lock: clearing the SPACER cell at (x+1, y)
        // must NOT clobber the WIDE_CHAR cell at (x, y). Test
        // pre-poisons NEITHER position; renders; asserts the wide
        // char is intact at col 0 + the spacer position is blank at
        // col 1. Without this lock a refactor that swapped the order
        // (clearing spacer BEFORE writing wide char) would silently
        // overwrite the wide char.
        let mut vt = VTerm::new(10, 1);
        vt.process("中".as_bytes());
        let area = ratatui::layout::Rect::new(0, 0, 10, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        vt.render_to_buffer(&mut buf, area, 0, false);
        assert_eq!(
            buf[(0, 0)].symbol(),
            "中",
            "wide char must be at col 0, got: {:?}",
            buf[(0, 0)].symbol()
        );
        assert_eq!(
            buf[(1, 0)].symbol(),
            " ",
            "spacer position at col 1 must be blank, got: {:?}",
            buf[(1, 0)].symbol()
        );
    }

    #[test]
    fn test_text_construction_paths_still_skip_spacers() {
        // #819 regression-proof for sites 2-5 (lines 331/365/415/492).
        // These are text/ANSI construction paths — they build fresh
        // String/Vec<u8> per call and their WIDE_CHAR_SPACER `continue`
        // is the CORRECT behavior (avoids inserting a placeholder space
        // into text representations consumers expect to be
        // WIDE_CHAR-collapsed). Locks the contract that the #819 fix
        // ONLY touched Site 1 (ratatui rendering); sites 2-5 unchanged.
        //
        // If a future refactor "extrapolates" the Site 1 fix to all 5
        // sites, the asserts below will fail (text output will gain
        // spurious spaces after wide chars), making the regression
        // visible. Per general's directive on the #819 dispatch.
        let mut vt = VTerm::new(20, 2);
        // Input: wide char + plain char + newline + plain text.
        // Expected text shape: "中A" (no space between 中 and A —
        // the wide char's SPACER position must NOT contribute to text).
        vt.process("中A\r\nB".as_bytes());

        // Site 4 (line 445) — tail_lines() (visible-rows text builder)
        let tail_text = vt.tail_lines(5);
        assert!(
            tail_text.contains("中A"),
            "tail_lines() must NOT insert space between wide char and next char, got: {tail_text:?}"
        );
        assert!(
            !tail_text.contains("中 A"),
            "tail_lines() must NOT insert SPACER as space, got: {tail_text:?}"
        );

        // Site 3 (line 395) — read_scrollback() (scrollback + visible)
        let scrollback = vt.read_scrollback(10);
        assert!(
            scrollback.contains("中A"),
            "read_scrollback() must NOT insert SPACER as space, got: {scrollback:?}"
        );
        assert!(
            !scrollback.contains("中 A"),
            "read_scrollback() leaked SPACER as space, got: {scrollback:?}"
        );

        // Site 2 (line 361) — extract_text() (selection text). max_scroll == 0
        // here, so logical line 0 == grid row 0.
        let selected = vt.extract_text((0, 0), (0, 5));
        assert!(
            selected.contains("中A"),
            "extract_text() must NOT insert SPACER as space, got: {selected:?}"
        );
        assert!(
            !selected.contains("中 A"),
            "extract_text() leaked SPACER as space, got: {selected:?}"
        );

        // Site 5 (line 522) — dump_screen() (ANSI escape sequence builder).
        // ANSI codes interleave between cells, so we don't assert
        // verbatim "中A" sequence — instead verify the SPACER never
        // contributes a literal space between the wide char and the
        // following char (the actual regression we're proofing).
        let ansi = vt.dump_screen();
        let ansi_str = String::from_utf8_lossy(&ansi);
        assert!(
            ansi_str.contains("中"),
            "dump_screen() must emit the wide char, got: {ansi_str:?}"
        );
        assert!(
            ansi_str.contains('A'),
            "dump_screen() must emit the following char, got: {ansi_str:?}"
        );
        assert!(
            !ansi_str.contains("中 A"),
            "dump_screen() leaked SPACER as literal space, got: {ansi_str:?}"
        );
    }

    // ── #1064 area-clamp residual-text class ──
    //
    // `render_to_buffer_inner` clamps writes to `rows × cols = min(self.rows,
    // area.height, grid_rows)` (same for cols). When `area > grid` — e.g.
    // resize race, initial spawn lag, outer terminal resize mid-frame — the
    // trailing rows/cols of `area` are never written. Combined with the
    // ratatui Buffer's cross-frame statefulness (codebase invariant from
    // #819), unwritten cells retain previous-frame content → operator-
    // observable residual text.
    //
    // These tests pre-poison cells outside the clamped region with sentinel
    // chars, render, and assert the cells are blanked. The fix is a
    // full-area pre-fill before the existing clamped-write loop.

    /// T1 (#1064): area taller than grid blanks the trailing rows.
    #[test]
    fn area_taller_than_grid_clears_trailing_rows() {
        let mut vt = VTerm::new(10, 3);
        vt.process(b"hello");
        let area = ratatui::layout::Rect::new(0, 0, 10, 5);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        for x in 0..10 {
            buf[(x, 3)].set_char('A');
            buf[(x, 4)].set_char('B');
        }
        vt.render_to_buffer(&mut buf, area, 0, false);
        let row3: String = (0..10).map(|x| buf[(x, 3)].symbol()).collect();
        let row4: String = (0..10).map(|x| buf[(x, 4)].symbol()).collect();
        assert_eq!(row3, "          ", "row 3 (beyond grid) must be blanked");
        assert_eq!(row4, "          ", "row 4 (beyond grid) must be blanked");
    }

    /// #1432: extract_text resolves absolute scrollback logical coordinates,
    /// stays correct under new output, and spans beyond the viewport.
    #[test]
    fn extract_text_logical_coords_stable_across_output() {
        let mut vt = VTerm::new(20, 3);
        for i in 0..10 {
            vt.process(format!("line{i}\r\n").as_bytes());
        }
        // Logical line index counts from the oldest buffer line: line{K} is at K.
        assert_eq!(vt.extract_text((5, 0), (5, 4)), "line5");
        // New output appends and scrolls the grid; the same logical anchor must
        // still extract line5 (selection tracks content, no drift).
        for i in 10..14 {
            vt.process(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(
            vt.extract_text((5, 0), (5, 4)),
            "line5",
            "logical anchor must survive appended output"
        );
        // A range spanning more than the 3-row viewport extracts every line —
        // selection can extend beyond the visible window.
        assert_eq!(
            vt.extract_text((2, 0), (7, 4)),
            "line2\nline3\nline4\nline5\nline6\nline7"
        );
    }

    /// T2 (#1064): area wider than grid blanks the trailing cols.
    #[test]
    fn area_wider_than_grid_clears_trailing_cols() {
        let mut vt = VTerm::new(5, 2);
        vt.process(b"abcde");
        let area = ratatui::layout::Rect::new(0, 0, 10, 2);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        for x in 5..10 {
            buf[(x, 0)].set_char('Z');
        }
        vt.render_to_buffer(&mut buf, area, 0, false);
        let tail: String = (5..10).map(|x| buf[(x, 0)].symbol()).collect();
        assert_eq!(tail, "     ", "cols beyond grid must be blanked");
    }

    /// T3 (#1064): resize-shrink leaves no orphan in trailing region.
    ///
    /// Simulates the resize race: grid was 20×10, render produced full
    /// content; grid shrinks to 10×5 but the pane area in the layout is
    /// still 20×10 (one frame behind). Trailing strip must be blanked.
    #[test]
    fn resize_shrink_clears_orphan_cells() {
        let mut vt = VTerm::new(20, 10);
        vt.process(b"line content that fills the wider grid view");
        let area = ratatui::layout::Rect::new(0, 0, 20, 10);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        vt.render_to_buffer(&mut buf, area, 0, false);
        // Capture pre-resize cell content (non-blank somewhere).
        let pre_resize_filled = (0..20).any(|x| (0..10).any(|y| buf[(x, y)].symbol() != " "));
        assert!(
            pre_resize_filled,
            "sanity: first render must populate cells"
        );

        // Shrink grid; render into same area (simulates layout lag).
        vt.resize(10, 5);
        vt.render_to_buffer(&mut buf, area, 0, false);
        // Bottom-right strip cols 10..20 × rows 5..10 must now be blank.
        for y in 5..10 {
            for x in 0..20 {
                assert_eq!(
                    buf[(x, y)].symbol(),
                    " ",
                    "({x},{y}) beyond new grid must be blanked"
                );
            }
        }
        for y in 0..5 {
            for x in 10..20 {
                assert_eq!(
                    buf[(x, y)].symbol(),
                    " ",
                    "({x},{y}) beyond new grid cols must be blanked"
                );
            }
        }
    }

    /// T4 (#1064): area == grid renders normally — no regression.
    #[test]
    fn area_equals_grid_renders_normally() {
        let mut vt = VTerm::new(10, 3);
        vt.process(b"hello");
        let area = ratatui::layout::Rect::new(0, 0, 10, 3);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        vt.render_to_buffer(&mut buf, area, 0, false);
        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(1, 0)].symbol(), "e");
        assert_eq!(buf[(4, 0)].symbol(), "o");
    }

    /// T5 (#1064): #819 WIDE_CHAR_SPACER invariant carries over.
    ///
    /// The pre-fill must not weaken the #819 fix. Pre-poison the spacer
    /// cell at col 1; render `中` at col 0; spacer at col 1 must be blank
    /// (per #819) AND the wide char at col 0 must be intact.
    #[test]
    fn wide_char_spacer_invariant_carries_with_prefill() {
        let mut vt = VTerm::new(10, 1);
        vt.process("中".as_bytes());
        let area = ratatui::layout::Rect::new(0, 0, 10, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        buf[(1, 0)].set_char('X');
        vt.render_to_buffer(&mut buf, area, 0, false);
        assert_eq!(buf[(0, 0)].symbol(), "中", "wide char preserved");
        assert_eq!(buf[(1, 0)].symbol(), " ", "spacer blanked per #819");
    }

    /// T6 (#1064, optional dev-2 nit): non-zero area origin still works.
    #[test]
    fn area_with_non_zero_origin_clears_trailing_region() {
        let mut vt = VTerm::new(5, 2);
        vt.process(b"ab");
        // Buffer covers (0,0,20,10); area is the sub-region (5,3,10,5).
        let buf_area = ratatui::layout::Rect::new(0, 0, 20, 10);
        let area = ratatui::layout::Rect::new(5, 3, 10, 5);
        let mut buf = ratatui::buffer::Buffer::empty(buf_area);
        // Pre-poison a cell inside `area` but outside the clamped grid region.
        buf[(12, 3)].set_char('R'); // col 12 = area-col 7, beyond grid cols=5
        buf[(8, 7)].set_char('S'); // row 7 = area-row 4, beyond grid rows=2
        vt.render_to_buffer(&mut buf, area, 0, false);
        assert_eq!(
            buf[(12, 3)].symbol(),
            " ",
            "non-zero origin: col 12 blanked"
        );
        assert_eq!(buf[(8, 7)].symbol(), " ", "non-zero origin: row 7 blanked");
        // Buffer cells OUTSIDE area must NOT be touched (sentinel at (0,0)).
        buf[(0, 0)].set_char('K');
        vt.render_to_buffer(&mut buf, area, 0, false);
        assert_eq!(
            buf[(0, 0)].symbol(),
            "K",
            "cells outside area must NOT be affected by pre-fill"
        );
    }

    /// T7 (#1064, optional dev-2 nit): zero-size area is a safe no-op.
    #[test]
    fn zero_size_area_is_safe_noop() {
        let mut vt = VTerm::new(10, 3);
        vt.process(b"hello");
        let area_zero_h = ratatui::layout::Rect::new(0, 0, 10, 0);
        let area_zero_w = ratatui::layout::Rect::new(0, 0, 0, 3);
        let buf_area = ratatui::layout::Rect::new(0, 0, 10, 3);
        let mut buf = ratatui::buffer::Buffer::empty(buf_area);
        buf[(0, 0)].set_char('K');
        // Either should panic-free + not touch any cells.
        vt.render_to_buffer(&mut buf, area_zero_h, 0, false);
        vt.render_to_buffer(&mut buf, area_zero_w, 0, false);
        assert_eq!(
            buf[(0, 0)].symbol(),
            "K",
            "zero-size area must not affect buffer"
        );
    }
}
