//! #2963: the `read_scrollback` test cluster, extracted verbatim from the
//! inline `vterm::tests` module so `src/vterm.rs` stays under its 3103-LOC
//! grandfathered ceiling (`tests/src_file_size_invariant.rs`). Move-only —
//! every test body and name is unchanged.

use super::VTerm;

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
fn read_scrollback_scans_only_bounded_tail_2963() {
    let mut vt = VTerm::new(80, 5);
    for i in 0..2_000 {
        vt.process(format!("line{i}\r\n").as_bytes());
    }

    let text = vt.read_scrollback(1);

    assert!(text.contains("line1999"), "latest line must be returned");
    assert!(
        vt.read_scrollback_rows.get() < 100,
        "read_scrollback(1) must not scan the retained history, visited {} rows",
        vt.read_scrollback_rows.get()
    );
}

#[test]
fn read_scrollback_preserves_blank_at_window_boundary_2963() {
    let mut vt = VTerm::new(80, 3);
    vt.process(b"A\r\n\r\nB");

    assert_eq!(vt.read_scrollback(2), "\nB");
}

#[test]
fn read_scrollback_trims_true_leading_blanks_at_top_2963() {
    let mut vt = VTerm::new(80, 3);
    vt.process(b"\r\n\r\nB");

    assert_eq!(vt.read_scrollback(10), "B");
}

#[test]
fn read_scrollback_trims_leading_blanks_before_full_window_2963() {
    let mut vt = VTerm::new(80, 3);
    vt.process(b"\r\n\r\nB");

    assert_eq!(vt.read_scrollback(2), "B");
}

#[test]
fn read_scrollback_zero_lines_does_not_scan() {
    let mut vt = VTerm::new(80, 5);
    vt.process(b"content\r\n");

    assert_eq!(vt.read_scrollback(0), "");
    assert_eq!(vt.read_scrollback_rows.get(), 0);
}

#[test]
fn read_scrollback_skips_wide_char_spacers() {
    let mut vt = VTerm::new(20, 2);
    vt.process("中A\r\nB".as_bytes());

    let text = vt.read_scrollback(10);

    assert!(text.contains("中A"), "wide char text must be preserved");
    assert!(
        !text.contains("中 A"),
        "wide char spacer must not add a space: {text:?}"
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
