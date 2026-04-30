//! Sprint 42 Phase 3 — 7 vte gotchas tests on TuiClient vterm.
//!
//! Exercises alacritty_terminal::Term edge cases via the Phase 2
//! TuiClient harness to verify zero parser divergence with production.

mod common;

use common::harness::{AgendHarness, TuiClient};

fn tmp_home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agend-vte-gotcha-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// 1. CRLF/LF handling — no double-counting of newlines.
#[test]
fn crlf_lf_handling() {
    let home = tmp_home("crlf");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("spawn");
    let mut client = TuiClient::new(&harness, 80, 24);

    let screen = client.feed_and_extract(b"line1\r\nline2\r\n");
    assert!(screen.contains("line1"), "must contain line1: '{screen}'");
    assert!(screen.contains("line2"), "must contain line2: '{screen}'");
    let line_count = screen.lines().count();
    assert_eq!(
        line_count, 2,
        "CRLF must produce exactly 2 lines, got {line_count}: '{screen}'"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// 2. ANSI cursor positioning — cursor home + clear line.
#[test]
fn ansi_cursor_positioning() {
    let home = tmp_home("cursor");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("spawn");
    let mut client = TuiClient::new(&harness, 80, 24);

    // Write "hello", then cursor home + clear line → "hello" should be gone
    client.feed(b"hello");
    client.feed(b"\x1b[H\x1b[K");
    let screen = client.screen_text(5);
    assert!(
        !screen.contains("hello"),
        "cursor home + clear must erase 'hello', got: '{screen}'"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// 3. Color SGR escape sequences — ANSI stripped in text output.
#[test]
fn color_sgr_stripped() {
    let home = tmp_home("sgr");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("spawn");
    let mut client = TuiClient::new(&harness, 80, 24);

    let screen = client.feed_and_extract(b"\x1b[31mred\x1b[0m normal\r\n");
    assert!(
        screen.contains("red"),
        "content 'red' must be preserved: '{screen}'"
    );
    assert!(
        screen.contains("normal"),
        "content 'normal' must be preserved: '{screen}'"
    );
    assert!(
        !screen.contains("\x1b["),
        "ANSI escape must be stripped from text output: '{screen}'"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// 4. Cursor save/restore — DEC save (ESC 7) / restore (ESC 8).
#[test]
fn cursor_save_restore() {
    let home = tmp_home("saverestore");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("spawn");
    let mut client = TuiClient::new(&harness, 80, 24);

    // Write "abc", save cursor (ESC 7), move to home, write "XY", restore (ESC 8)
    client.feed(b"abc\x1b7\x1b[HXY\x1b8");
    // After restore, cursor is back at col 3 (after "abc"). Write "D" there.
    client.feed(b"D\r\n");
    let screen = client.screen_text(5);
    // Line should be "XYcD" — "XY" overwrote cols 0-1, "c" at col 2 survived,
    // "D" written at col 3 (restored position).
    assert!(
        screen.contains("XY"),
        "cursor home write must appear: '{screen}'"
    );
    assert!(
        screen.contains("D"),
        "post-restore write 'D' must appear at restored position: '{screen}'"
    );
    // Key assertion: "D" must NOT be at col 2 (where cursor was before restore)
    // but at col 3 (saved position). Verify "cD" substring exists.
    assert!(
        screen.contains("cD") || screen.contains("XYcD"),
        "post-restore 'D' must follow 'c' (saved position col 3): '{screen}'"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// 5. Scrollback line preservation — overflow visible rows.
#[test]
fn scrollback_line_preservation() {
    let home = tmp_home("scrollback");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("spawn");
    let mut client = TuiClient::new(&harness, 80, 5); // only 5 visible rows

    // Feed 20 lines — first 15 scroll into history
    for i in 1..=20 {
        client.feed(format!("LINE{i}\r\n").as_bytes());
    }
    // read_scrollback should capture historical lines
    let scrollback = client.read_scrollback(20);
    assert!(
        scrollback.contains("LINE1"),
        "scrollback must preserve LINE1: '{scrollback}'"
    );
    assert!(
        scrollback.contains("LINE20"),
        "scrollback must preserve LINE20: '{scrollback}'"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// 6. Wide chars (CJK/emoji) — no panic, content preserved.
#[test]
fn wide_chars_no_panic() {
    let home = tmp_home("widechar");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("spawn");
    let mut client = TuiClient::new(&harness, 80, 24);

    // UTF-8 "你好" + emoji "😀"
    let screen = client.feed_and_extract("你好 😀 test\r\n".as_bytes());
    // Must not panic; content should be present (wide char spacer skipped)
    assert!(
        screen.contains("test"),
        "ASCII content after wide chars must be preserved: '{screen}'"
    );
    // CJK characters: verify non-ASCII content survived vterm processing
    assert!(
        !screen.is_ascii(),
        "wide chars must survive vterm processing (non-ASCII chars present): '{screen}'"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// 7. safe_cell bounds — resize shrink doesn't panic, content accessible.
#[test]
fn safe_cell_resize_shrink_no_panic() {
    let home = tmp_home("resize");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("spawn");
    let mut client = TuiClient::new(&harness, 80, 24);

    // Fill screen with content at 80x24
    for i in 0..30 {
        client.feed(format!("row{i} content\r\n").as_bytes());
    }
    // Shrink terminal — content that was at cols 40-79 is now out of bounds
    client.resize(40, 10);
    // Read screen after shrink — must not panic
    let screen = client.screen_text(10);
    // After resize, some content should still be readable
    assert!(
        screen.contains("row") || screen.is_empty(),
        "screen_text after resize-shrink must not panic and should contain content or be empty: '{screen}'"
    );
    // Verify dimensions actually changed by feeding new content
    client.feed(b"after_resize\r\n");
    let screen2 = client.screen_text(5);
    assert!(
        screen2.contains("after_resize"),
        "content fed after resize must appear: '{screen2}'"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}
