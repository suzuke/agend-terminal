use super::{safe_cell, VTerm};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::Flags;

impl VTerm {
    /// Match a suffix only against the visible text ending at the live cursor.
    ///
    /// The current row is read only through the cursor insertion column. Earlier
    /// rows are included only while alacritty proves that the immediately previous
    /// row soft-wrapped via `WRAPLINE`; hard rows and scrollback are never joined.
    /// `max_rows` bounds the backwards walk and is supplied from the suffix width.
    pub(crate) fn cursor_anchored_suffix_matches(&self, suffix: &str, max_rows: usize) -> bool {
        if suffix.is_empty() || max_rows == 0 || self.cols == 0 || self.rows == 0 {
            return false;
        }

        let grid = self.term.grid();
        let cursor = grid.cursor.point;
        let Ok(cursor_row) = usize::try_from(cursor.line.0) else {
            return false;
        };
        let cols = self.cols.min(grid.columns() as u16) as usize;
        let rows = self.rows.min(grid.screen_lines() as u16) as usize;
        if cols == 0 || cursor_row >= rows {
            return false;
        }
        let cursor_col = cursor.column.0.min(cols);

        let mut start_row = cursor_row;
        let mut included_rows = 1usize;
        while start_row > 0 && included_rows < max_rows {
            let previous_last = safe_cell(grid, Line((start_row - 1) as i32), cols - 1);
            if !previous_last.flags.contains(Flags::WRAPLINE) {
                break;
            }
            start_row -= 1;
            included_rows += 1;
        }

        let mut candidate = String::new();
        for row in start_row..=cursor_row {
            let end_col = if row == cursor_row { cursor_col } else { cols };
            for col in 0..end_col {
                let cell = safe_cell(grid, Line(row as i32), col);
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                candidate.push(if cell.c == '\0' { ' ' } else { cell.c });
            }
        }

        candidate.ends_with(suffix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Point};

    #[test]
    fn cursor_suffix_uses_wrapline_not_row_fullness_3175() {
        let mut wrapped = VTerm::new(10, 3);
        wrapped.process(b"abc\r\ndef");
        wrapped.term.grid_mut()[Point::new(Line(0), Column(9))]
            .flags
            .insert(Flags::WRAPLINE);
        assert!(wrapped.cursor_anchored_suffix_matches("abc       def", 13));

        let mut hard = VTerm::new(10, 3);
        hard.process(b"1234567890\r\ntail");
        assert!(!hard.cursor_anchored_suffix_matches("1234567890tail", 14));
    }
}
