//! #3330: the `tail_lines_core` CJK-allocation tests, split out of the inline
//! `vterm::tests` module so `src/vterm.rs` stays under its 3103-LOC grandfathered
//! ceiling (`tests/src_file_size_invariant.rs`). Move-only — every test body and
//! name is unchanged from the commit that introduced them.

use super::{tail_lines_core, CellView};
use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// #cjk-render-cost regression: `tail_lines_core` preallocated its row buffer with
/// `String::with_capacity(cols)` — a COLUMN count used as a BYTE capacity. This test
/// pins the arithmetic that made that wrong: a full-width CJK row occupies `cols`
/// columns but needs MORE than `cols` bytes, so the old preallocation could never
/// hold it and every such row paid a realloc + memmove. Measured cost: the render
/// branch of the TUI main loop went from 2.6% of samples while typing ASCII to
/// 28.5% while typing CJK (~11x) on the same pane.
#[test]
fn tail_lines_core_row_buffer_must_hold_a_full_width_cjk_row() {
    let cols = 80;
    let (text, _, _) = tail_lines_core(cols, 1, 1, false, false, |_, col| CellView {
        // even column = the wide char, odd column = its spacer
        c: if col % 2 == 0 { '中' } else { ' ' },
        fg: Color::Named(NamedColor::Foreground),
        dim: false,
        wrapline: false,
        wide_spacer: col % 2 == 1,
    });
    assert_eq!(
        text.chars().count(),
        cols / 2,
        "one wide char per two columns"
    );
    assert!(
        text.len() > cols,
        "a full-width CJK row is {} bytes across {cols} columns — \
         `String::with_capacity(cols)` could not hold it without reallocating",
        text.len()
    );
}

/// The allocation fix must not change what `tail_lines_core` returns. Covers the
/// paths the rewrite touched: trailing-space trimming (now done by truncating in
/// place instead of `trim_end().to_string()`), the wrapped-row exemption that keeps
/// a significant trailing space, and fg/dim truncation alignment on a MIXED-width
/// row (where byte length and char count diverge — exactly the case the single
/// `chars().count()` now has to get right).
#[test]
fn tail_lines_core_output_unchanged_by_the_allocation_fix() {
    let fgc = Color::Named(NamedColor::Foreground);
    // Row content: "a中b" then trailing spaces. cols wide, no wrap.
    let row: Vec<(char, bool)> = {
        let mut v = vec![('a', false), ('中', false), (' ', true), ('b', false)];
        v.resize(10, (' ', false));
        v
    };
    let (text, fg, dim) = tail_lines_core(10, 1, 1, true, false, |_, col| {
        let (c, spacer) = row[col];
        CellView {
            c,
            fg: fgc,
            dim: false,
            wrapline: false,
            wide_spacer: spacer,
        }
    });
    assert_eq!(text, "a中b", "trailing blanks trimmed, wide spacer skipped");
    assert_eq!(
        fg.len(),
        text.chars().count(),
        "fg must be truncated to the CHAR count (3), not the byte length (5)"
    );
    assert_eq!(dim.len(), text.chars().count());

    // Wrapped row: the trailing space is significant and must survive.
    let (wrapped_text, _, _) = tail_lines_core(4, 1, 1, false, true, |_, col| CellView {
        c: if col == 0 { 'x' } else { ' ' },
        fg: fgc,
        dim: false,
        wrapline: col == 3,
        wide_spacer: false,
    });
    assert_eq!(
        wrapped_text, "x   ",
        "a soft-wrapped row keeps its trailing space"
    );
}
