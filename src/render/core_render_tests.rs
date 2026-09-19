#![cfg(test)]

use super::*;
use crate::layout::{Pane, PaneSource};
use crate::vterm::VTerm;

#[test]
fn badge_shows_pending_count() {
    let pane = Pane {
        agent_name: "agent".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 3,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    let segments = pane_title_segments(&pane, Style::default(), Some(AgentState::Idle), false);
    let joined = segments
        .into_iter()
        .map(|(text, _)| text)
        .collect::<String>();
    assert!(joined.contains("[3]"));
}

#[test]
fn pane_title_shows_pending_decision_marker_2313() {
    let pane = Pane {
        agent_name: "agent".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 1,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    let segments = pane_title_segments(&pane, Style::default(), Some(AgentState::Idle), false);
    let joined = segments
        .into_iter()
        .map(|(text, _)| text)
        .collect::<String>();
    assert!(
        joined.contains('🔴'),
        "pane authoring a pending decision must show the marker: {joined}"
    );
}

#[test]
fn pane_title_decision_marker_inherits_label_background_pane_label_bg_lost() {
    let pane = Pane {
        agent_name: "agent".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 1,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    // Mirrors `render_pane`'s real focused-pane `title_style` construction
    // (a solid background band, black text, bold) — the two prior tests
    // above pass `Style::default()` (no background), which can't catch a
    // background-inheritance regression because `Style::default()` has no
    // background to inherit in the first place.
    let title_style = Style::default()
        .bg(Color::Cyan)
        .fg(Color::Black)
        .add_modifier(Modifier::BOLD);
    let segments = pane_title_segments(&pane, title_style, Some(AgentState::Idle), false);
    let (_, icon_style) = segments
        .iter()
        .find(|(text, _)| text.contains('🔴'))
        .expect("pending decision marker segment must be present");
    assert_eq!(
        icon_style.bg,
        Some(Color::Cyan),
        "the 🔴 marker must inherit the surrounding title's background, \
         not fall back to the terminal default (breaks the visually \
         continuous label band)"
    );
    assert_eq!(
        icon_style.fg,
        Some(Color::Red),
        "the marker itself must stay red (only the background should \
         inherit from the title)"
    );
}

#[test]
fn pane_title_no_decision_marker_when_zero_2313() {
    let pane = Pane {
        agent_name: "agent".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    let segments = pane_title_segments(&pane, Style::default(), Some(AgentState::Idle), false);
    let joined = segments
        .into_iter()
        .map(|(text, _)| text)
        .collect::<String>();
    assert!(
        !joined.contains('🔴'),
        "no pending decision → no marker: {joined}"
    );
}

#[test]
fn pane_title_no_state_suffix() {
    let pane = Pane {
        agent_name: "agent".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: Some(crate::backend::Backend::ClaudeCode),
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    // #1713 flag OFF (default): no state badge appended; only the base label
    // (+ the transient Restarting/Crashed tab badge, which lives elsewhere).
    let segments = pane_title_segments(&pane, Style::default(), Some(AgentState::Idle), false);
    let joined: String = segments.iter().map(|(t, _)| t.as_str()).collect();
    assert!(
        !joined.contains("[Idle]") && !joined.contains("[idle]"),
        "flag-off: pane title must not contain a state badge, got: {joined}"
    );

    let unknown = pane_title_segments(&pane, Style::default(), None, false);
    let unknown_joined: String = unknown.iter().map(|(t, _)| t.as_str()).collect();
    assert!(
        unknown_joined.contains("[?]"),
        "unknown remote state must be visible by default, got: {unknown_joined}"
    );
}

/// #1713 flag ON: the pane title appends a `[<State>]` badge of the detected
/// AgentState (all states), so the operator can eyeball-verify detection.
#[test]
fn pane_title_state_badge_when_flag_on_1713() {
    let pane = Pane {
        agent_name: "agent".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: Some(crate::backend::Backend::ClaudeCode),
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    for (state, want) in [
        (AgentState::ServerRateLimit, "[ServerRateLimit]"),
        (AgentState::PermissionPrompt, "[PermissionPrompt]"),
        (AgentState::Active, "[Active]"),
        (AgentState::Idle, "[Idle]"),
    ] {
        let segments = pane_title_segments(&pane, Style::default(), Some(state), true);
        let joined: String = segments.iter().map(|(t, _)| t.as_str()).collect();
        assert!(
            joined.contains(want),
            "flag-on: title must contain {want}, got: {joined}"
        );
    }
}

/// #3670 RED: a correlated restart failure must be visible in the affected
/// pane title, not only in logs or a global overlay.
#[test]
fn pane_title_shows_correlated_restart_failure_3670() {
    let mut pane = Pane {
        agent_name: "agent".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    pane.set_restart_error("spawn failed");

    let segments = pane_title_segments(&pane, Style::default(), Some(AgentState::Idle), false);
    let joined: String = segments.iter().map(|(text, _)| text.as_str()).collect();
    assert!(
        joined.contains("[RESTART FAILED:") && joined.contains("spawn failed"),
        "pane title must expose the correlated restart failure: {joined}"
    );
}

#[test]
fn state_color_returns_distinct_colors_for_key_states() {
    let idle = state_color(AgentState::Idle);
    let active = state_color(AgentState::Active);
    assert_ne!(idle, active, "idle vs active must differ");
}

#[test]
fn state_color_error_states_are_red() {
    assert_eq!(state_color(AgentState::Crashed), Color::Red);
    assert_eq!(state_color(AgentState::Restarting), Color::Red);
}

#[test]
fn highest_priority_state_returns_unknown_for_empty_tab() {
    let tab = crate::layout::Tab::new(
        "empty".to_string(),
        crate::layout::Pane {
            agent_name: "test".into(),
            instance_id: crate::types::InstanceId::default(),
            instance_ref: None,
            vterm: VTerm::new(10, 10),
            rx: crossbeam_channel::bounded(1).1,
            id: 1,
            backend: None,
            working_dir: None,
            display_name: None,
            restart_error: None,
            scroll_offset: 0,
            has_notification: false,
            fleet_instance_name: None,
            last_input_at: None,
            pending_notification_count: 0,
            pending_decision_count: 0,
            selection: None,
            source: PaneSource::Local,
            offthread: None,
            _fwd_cancel: None,
        },
    );
    let snapshot = HashMap::new();
    let result = highest_priority_state(&tab, &snapshot);
    assert_eq!(result, None);
}

#[test]
fn render_resizes_vterm_to_pane_content_rows_2046() {
    let backend = ratatui::backend::TestBackend::new(40, 20);
    let mut terminal =
        ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
    let registry: AgentRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

    let pane = Pane {
        agent_name: "agent".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        // 40x20 frame -> pane tree is 40x18 after tab/status chrome, and
        // pane border leaves a 38x16 terminal content area. Start 5 rows
        // short to reproduce #2046's floating backend footer symptom.
        vterm: VTerm::new(38, 11),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    let mut layout = Layout::new();
    layout.add_tab(crate::layout::Tab::new("agent".to_string(), pane));

    terminal
        .draw(|frame| {
            render(
                frame,
                &mut layout,
                false,
                &registry,
                TelegramStatus::NotConfigured,
                false,
                0,
                crate::runtime::AgentListMode::Live,
                None,
            );
        })
        .expect("test terminal draw should succeed");

    let pane = layout.active_tab().unwrap().focused_pane().unwrap();
    assert_eq!(pane.vterm.cols(), 38);
    assert_eq!(
        pane.vterm.rows(),
        16,
        "render must keep the VTerm/PTY rows equal to pane content rows"
    );
}

/// Option X (S3 wiring): when `pane.offthread = Some`, `render_pane` paints the
/// parser thread's published snapshot — NOT the (idle) main-thread `pane.vterm`.
/// Proven by leaving `pane.vterm` blank and asserting the snapshot's content
/// reaches the frame buffer. (Pairs with `drain_output_is_noop_when_offthread...`
/// in layout::pane: together they show the off-thread path renders correctly
/// while the main thread does zero parse.)
#[test]
fn render_paints_offthread_snapshot_not_main_vterm() {
    // Spawn a parser, push known content, and wait for it to publish a snapshot.
    let (data_tx, data_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (wake_tx, wake_rx) = crossbeam_channel::unbounded::<usize>();
    let handle = crate::render::offthread::spawn_offthread_parser(
        1,
        "t".to_string(),
        data_rx,
        VTerm::new(38, 16),
        wake_tx,
    )
    .expect("parser thread spawns");
    data_tx
        .send(b"\x1b[2J\x1b[HOFFTHREAD_SNAP".to_vec())
        .unwrap();
    wake_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("parser thread must publish a snapshot");

    let backend = ratatui::backend::TestBackend::new(40, 20);
    let mut terminal =
        ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
    let registry: AgentRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

    let pane = Pane {
        agent_name: "agent".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        // Blank/idle — the render source MUST be the snapshot, not this VTerm.
        vterm: VTerm::new(38, 16),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: Some(handle),
        _fwd_cancel: None,
    };
    assert!(
        pane.vterm.tail_lines(16).trim().is_empty(),
        "sanity: the main-thread VTerm starts blank"
    );

    let mut layout = Layout::new();
    layout.add_tab(crate::layout::Tab::new("agent".to_string(), pane));
    terminal
        .draw(|frame| {
            render(
                frame,
                &mut layout,
                false,
                &registry,
                TelegramStatus::NotConfigured,
                false,
                0,
                crate::runtime::AgentListMode::Live,
                None,
            );
        })
        .expect("test terminal draw should succeed");

    let buf = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
    }
    assert!(
        text.contains("OFFTHREAD_SNAP"),
        "render must paint the off-thread snapshot content, not the blank main VTerm; frame: {text:?}"
    );
}

#[test]
fn main_tui_footer_shows_help_hint() {
    let backend = ratatui::backend::TestBackend::new(100, 3);
    let mut terminal =
        ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
    let layout = crate::layout::Layout::new();
    terminal
        .draw(|frame| {
            render_status_bar(
                frame,
                frame.area(),
                &layout,
                TelegramStatus::NotConfigured,
                false,
                0,
                crate::runtime::AgentListMode::Live,
            );
        })
        .expect("test terminal draw should succeed");
    let buf = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
    }
    assert!(
        text.contains("Ctrl+B ?"),
        "status bar should contain 'Ctrl+B ?' hint, got: {text}"
    );
}

/// #1140: a wide (2-cell) char replaced by narrow chars across frames must
/// leave no ghost in the former spacer cell. Contract-lock for the
/// wide→narrow render path our panes depend on.
///
/// Note: the plain-CJK case already worked on ratatui 0.29 — the #1140
/// ghost came from VS16-emoji width miscalculation, which the 0.30 upgrade
/// fixes. That bug is layout-level (width-dependent placement), so it can't
/// be cleanly discriminated cross-version in a unit test; the emoji case
/// below documents the correct 0.30 behavior and the real-world ghost is
/// confirmed by operator visual check (see PR).
fn render_row(terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>, text: &str) {
    terminal
        .draw(|frame| {
            frame
                .buffer_mut()
                .set_string(0, 0, text, ratatui::style::Style::default());
        })
        .expect("test draw should succeed");
}

fn backend_row(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>, width: u16) -> String {
    let buf = terminal.backend().buffer();
    (0..width)
        .map(|x| buf.cell((x, 0)).map(|c| c.symbol()).unwrap_or(" "))
        .collect()
}

#[test]
fn wide_char_to_narrow_leaves_no_ghost() {
    let backend = ratatui::backend::TestBackend::new(4, 1);
    let mut terminal =
        ratatui::Terminal::new(backend).expect("test terminal creation should succeed");

    // Frame 1: wide "中" spans cols 0-1, "X" at col 2.
    render_row(&mut terminal, "中X ");
    // Frame 2: "中" replaced by narrow "ab".
    render_row(&mut terminal, "abX ");
    assert_eq!(
        backend_row(&terminal, 4).trim_end(),
        "abX",
        "spacer cell must not retain the wide char's right half"
    );

    // Issue's exact symptom: lone narrow char where a wide char was, rest empty.
    render_row(&mut terminal, "中X ");
    render_row(&mut terminal, "a   ");
    assert_eq!(
        backend_row(&terminal, 4).trim_end(),
        "a",
        "no lone-char ghost may persist in the former spacer cell"
    );

    // VS16 emoji (U+2764 U+FE0F) — width-2 on ratatui 0.30, the actual
    // #1140 ghost source. Documents correct post-upgrade clearing.
    render_row(&mut terminal, "\u{2764}\u{fe0f}X ");
    render_row(&mut terminal, "abX ");
    assert_eq!(
        backend_row(&terminal, 4).trim_end(),
        "abX",
        "VS16-emoji spacer must be cleared on wide→narrow"
    );
}

/// #1027 RED: when `binary_stale` is true, the status bar MUST show
/// a "daemon binary stale" warning so the operator sees a stable
/// TUI indicator (replacing the previous inbox-emit path which
/// targeted agents who cannot act on it).
#[test]
fn status_bar_shows_warning_when_binary_stale() {
    let backend = ratatui::backend::TestBackend::new(120, 3);
    let mut terminal =
        ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
    let layout = crate::layout::Layout::new();
    terminal
        .draw(|frame| {
            render_status_bar(
                frame,
                frame.area(),
                &layout,
                TelegramStatus::NotConfigured,
                true,
                0,
                crate::runtime::AgentListMode::Live,
            );
        })
        .expect("test terminal draw should succeed");
    let buf = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
    }
    assert!(
        text.contains("daemon binary stale"),
        "binary_stale=true must surface a warning in status bar, got: {text}"
    );
}

/// #1027 RED: when `binary_stale` is false, no warning is shown.
#[test]
fn status_bar_no_warning_when_binary_fresh() {
    let backend = ratatui::backend::TestBackend::new(120, 3);
    let mut terminal =
        ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
    let layout = crate::layout::Layout::new();
    terminal
        .draw(|frame| {
            render_status_bar(
                frame,
                frame.area(),
                &layout,
                TelegramStatus::NotConfigured,
                false,
                0,
                crate::runtime::AgentListMode::Live,
            );
        })
        .expect("test terminal draw should succeed");
    let buf = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
    }
    assert!(
        !text.contains("daemon binary stale"),
        "binary_stale=false must NOT surface warning, got: {text}"
    );
}

#[test]
fn status_bar_surfaces_daemon_fallback_mode() {
    let backend = ratatui::backend::TestBackend::new(160, 3);
    let mut terminal = ratatui::Terminal::new(backend).expect("create test terminal");
    let layout = crate::layout::Layout::new();
    terminal
        .draw(|frame| {
            render_status_bar(
                frame,
                frame.area(),
                &layout,
                TelegramStatus::NotConfigured,
                false,
                0,
                crate::runtime::AgentListMode::FallbackDaemonAbsent,
            );
        })
        .expect("draw status bar");
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        text.contains("fallback — no daemon detected"),
        "got: {text}"
    );
}

#[test]
fn status_bar_shows_pending_decisions_badge_2313() {
    let backend = ratatui::backend::TestBackend::new(120, 3);
    let mut terminal =
        ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
    let layout = crate::layout::Layout::new();
    terminal
        .draw(|frame| {
            render_status_bar(
                frame,
                frame.area(),
                &layout,
                TelegramStatus::NotConfigured,
                false,
                2,
                crate::runtime::AgentListMode::Live,
            );
        })
        .expect("test terminal draw should succeed");
    let buf = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
    }
    assert!(
        text.contains("2 decisions pending"),
        "pending_decisions=2 must surface the badge, got: {text}"
    );
}

#[test]
fn status_bar_no_decisions_badge_when_zero_2313() {
    let backend = ratatui::backend::TestBackend::new(120, 3);
    let mut terminal =
        ratatui::Terminal::new(backend).expect("test terminal creation should succeed");
    let layout = crate::layout::Layout::new();
    terminal
        .draw(|frame| {
            render_status_bar(
                frame,
                frame.area(),
                &layout,
                TelegramStatus::NotConfigured,
                false,
                0,
                crate::runtime::AgentListMode::Live,
            );
        })
        .expect("test terminal draw should succeed");
    let buf = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
    }
    assert!(
        !text.contains("decisions pending"),
        "pending_decisions=0 must NOT surface the badge, got: {text}"
    );
}

#[test]
fn split_chunks_tiny_terminal_no_underflow() {
    use ratatui::layout::Rect;
    let area = Rect::new(0, 0, 1, 1);
    let [_a, b] = crate::layout::split_chunks(area, &crate::layout::SplitDir::Horizontal, 0.9);
    assert!(
        b.height >= 1,
        "second chunk height must be ≥1, got {}",
        b.height
    );
    let [_c, d] = crate::layout::split_chunks(area, &crate::layout::SplitDir::Vertical, 0.9);
    assert!(
        d.width >= 1,
        "second chunk width must be ≥1, got {}",
        d.width
    );
}

// ── #1071 chrome Clear-widget pre-render tests ──
//
// Reviewer RC0 caught that `Block::default().style(...)` does NOT clear
// cells (verified in ratatui-0.29 source: Block::render_ref calls
// `buf.set_style` which is style-only). RC1 uses `Clear` widget which
// calls `cell.reset()` per cell — properly blanking before Paragraph
// overlay. Tests pin the production wiring at 3 chrome sites:
//   1. render_tab_bar
//   2. render_status_bar (single Clear before both left + right bars)
//   3. "No agents" fallback Paragraph
//
// T1/T1c/T2 are STRUCTURAL pins (include_str + grep) — they FAIL on
// pre-#1071 source and PASS post-fix. T3 is a behavioral sanity test
// documenting the Clear-then-Paragraph contract (passes both ways).
// T4 references #1064/#1066 VTerm pre-fill (already shipped). T5
// documents the AGEND_RENDER_DEBUG diagnostic separately per reviewer.

/// T1 (#1071 RED): render_tab_bar source must `render_widget(Clear, …)`
/// before the Paragraph render. Pre-fix code only has the bare
/// Paragraph render at this site.
#[test]
fn render_tab_bar_uses_clear_widget_pre_render() {
    let source = include_str!("core_render.rs");
    let prod_end = source
        .find("#[cfg(test)]")
        .expect("core_render.rs must have a #[cfg(test)] tests module");
    let prod_src = &source[..prod_end];
    let tab_bar_start = prod_src
        .find("fn render_tab_bar")
        .expect("fn render_tab_bar must exist");
    let tab_bar_end = prod_src[tab_bar_start..]
        .find("\nfn ")
        .map(|i| tab_bar_start + i)
        .unwrap_or(prod_src.len());
    let body = &prod_src[tab_bar_start..tab_bar_end];
    assert!(
        body.contains("Clear"),
        "#1071 invariant: render_tab_bar must render Clear widget before Paragraph"
    );
}

/// T1c (#1071 RED, dev-2 nit): "No agents" fallback Paragraph must be
/// preceded by Clear render. Pre-fix code has only the fallback
/// Paragraph render with no Clear.
#[test]
fn no_agents_fallback_uses_clear_widget_pre_render() {
    let source = include_str!("core_render.rs");
    let prod_end = source
        .find("#[cfg(test)]")
        .expect("core_render.rs must have a #[cfg(test)] tests module");
    let prod_src = &source[..prod_end];
    // The fallback Paragraph contains a distinctive literal.
    let no_agents_pos = prod_src
        .find("No agents. Press Ctrl+B c to create a new tab.")
        .expect("\"No agents.\" fallback must exist");
    // Search backwards for the preceding fn boundary to scope the body.
    let scope_start = prod_src[..no_agents_pos]
        .rfind("None => {")
        .expect("fallback must be inside the None branch");
    let scope_end = no_agents_pos + "No agents. Press Ctrl+B c to create a new tab.".len() + 200; // enough room past the Paragraph render to catch the call sequence
    let body = &prod_src[scope_start..scope_end.min(prod_src.len())];
    assert!(
        body.contains("Clear"),
        "#1071 invariant: \"No agents\" fallback must render Clear widget before fallback Paragraph"
    );
}

/// T2 (#1071 RED): render_status_bar source must `render_widget(Clear,
/// …)` before any Paragraph render. Pre-fix code has only the
/// left+right bar Paragraphs with no Clear.
#[test]
fn render_status_bar_uses_clear_widget_pre_render() {
    let prod_src = include_str!("team_render.rs");
    let status_start = prod_src
        .find("fn render_status_bar")
        .expect("fn render_status_bar must exist");
    let status_end = prod_src[status_start..]
        .find("\nfn ")
        .map(|i| status_start + i)
        .unwrap_or(prod_src.len());
    let body = &prod_src[status_start..status_end];
    assert!(
        body.contains("Clear"),
        "#1071 invariant: render_status_bar must render Clear widget before Paragraph(s)"
    );
    // Also pin: Clear must appear BEFORE the first Paragraph render.
    let clear_pos = body.find("Clear").unwrap_or(body.len());
    let first_para = body
        .find("Paragraph::new")
        .expect("status bar must construct a Paragraph");
    assert!(
        clear_pos < first_para,
        "#1071 invariant: Clear must precede Paragraph render in status_bar; \
         got Clear at {clear_pos}, first Paragraph at {first_para}"
    );
}

/// T3 (#1071 sanity): Clear widget followed by Paragraph blanks
/// pre-poisoned cells outside the Paragraph's span content. Documents
/// the contract; passes both pre-fix and post-fix.
#[test]
fn clear_then_paragraph_blanks_residual_outside_spans() {
    use ratatui::widgets::{Clear, Widget};
    let area = Rect::new(0, 0, 30, 1);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    for x in 0..30 {
        buf[(x, 0)].set_char('X');
    }
    Widget::render(Clear, area, &mut buf);
    let para = Paragraph::new(Line::from(vec![Span::raw("short")]))
        .style(Style::default().bg(Color::DarkGray));
    Widget::render(para, area, &mut buf);
    let tail: String = (5..30).map(|x| buf[(x, 0)].symbol()).collect();
    let expected: String = " ".repeat(25);
    assert_eq!(
        tail, expected,
        "Clear+Paragraph must blank trailing cells, got: {tail:?}"
    );
}

// T4 (#1071 reference): VTerm body residual is locked by PR #1066
// (#1064 fix) via `src/vterm.rs::tests::area_taller_than_grid_*` + siblings.
// Chrome layer (this PR) and VTerm body layer cover disjoint regions:
// tab bar at top row, status bar at bottom row, VTerm body in the middle
// Min(1) region. No new test added here.

// T5 (#1071 separate concern per reviewer): AGEND_RENDER_DEBUG diagnostic
// env flag is preserved as standalone debug-gate, NOT part of the main fix.
// Operator can run the daemon with `AGEND_RENDER_DEBUG=1` to call
// `terminal.clear()` before each draw, distinguishing chrome-buffer-level
// residual (would disappear under the diagnostic) from alacritty-grid-level
// residual (would persist; points at H8 backend partial-redraw class).
#[test]
#[ignore = "diagnostic env-gated; runs manually with AGEND_RENDER_DEBUG=1"]
fn render_debug_env_diagnostic_documented() {
    // Placeholder: the diagnostic gate is implementation-tracked as a
    // separate concern. This test documents the protocol for future
    // wiring; runs only with `cargo test -- --ignored` against a daemon
    // spun up with the env set.
}

/// #2413 (A): the badge picks the Shadow Observer correction over the raw state
/// ONLY when `show_observed` AND a correction is published; otherwise the raw
/// `published_state` wins. Pins all three branches of `observed_or_raw_state` —
/// the lock-free read the render snapshot uses. `#[cfg(unix)]` (mk_test_handle).
#[cfg(unix)]
#[test]
fn observed_or_raw_state_prefers_correction_only_when_enabled() {
    let id = crate::types::InstanceId::default();
    let handle = crate::agent::mk_test_handle("agent", id);
    // Raw screen state = Restarting (via record_set); a published badge override.
    handle.core.lock().state.set_restarting();
    handle
        .core
        .lock()
        .state
        .publish_observed(Some(AgentState::Active));

    // Toggle ON ⇒ the high-confidence correction wins.
    assert_eq!(observed_or_raw_state(&handle, true), AgentState::Active);
    // Toggle OFF ⇒ the raw state wins (operator opted out / observer killed).
    assert_eq!(
        observed_or_raw_state(&handle, false),
        AgentState::Restarting
    );

    // No correction published (sentinel) ⇒ raw state even when enabled.
    handle.core.lock().state.publish_observed(None);
    assert_eq!(observed_or_raw_state(&handle, true), AgentState::Restarting);
}

/// Regression for the post-#2346 residual freeze: the per-frame render state
/// snapshot must read each agent's state via the lock-free published mirror,
/// NOT `core.lock()`. Under the boot PTY flood the core lock is held multi-ms
/// by each `pty_read_loop` feed; when the snapshot took it, the render loop
/// (and thus input) stalled up to ~10 ms/frame. Here we hold an agent's core
/// lock on a background thread and assert the snapshot still returns promptly
/// AND with the correct published state. If `build_agent_state_snapshot` ever
/// reverts to `core.lock().state.get_state()`, this blocks ~200 ms and fails.
///
/// `#[cfg(unix)]`: the only registry-handle builder, `agent::mk_test_handle`,
/// is `#[cfg(all(test, unix))]` (real openpty + `true`), so this test is
/// unix-only. The lock-free property itself is platform-agnostic; the state
/// unit tests (`agentstate_u8_roundtrip`, `published_mirror_tracks_current_…`)
/// cover the mirror cross-platform.
#[cfg(unix)]
#[test]
fn snapshot_reads_published_state_without_core_lock() {
    let id = crate::types::InstanceId::default();
    let handle = crate::agent::mk_test_handle("agent", id);
    // Drive a real transition through record_set so the published mirror moves
    // off its initial value.
    handle.core.lock().state.set_restarting();
    let core = std::sync::Arc::clone(&handle.core);

    let registry: AgentRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::from([
            (id, handle),
        ])));

    let pane = Pane {
        agent_name: "agent".into(),
        instance_id: id,
        instance_ref: None,
        vterm: VTerm::new(38, 11),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: Some(crate::backend::Backend::ClaudeCode),
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    let mut layout = Layout::new();
    layout.add_tab(crate::layout::Tab::new("agent".to_string(), pane));

    // Hold the agent's core lock on a background thread; signal once held so
    // the timing assertion is deterministic (no sleep-race).
    let (tx, rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _g = core.lock();
        tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
    });
    rx.recv().unwrap(); // core lock is now held

    let t0 = std::time::Instant::now();
    let snap = build_agent_state_snapshot(&layout, &registry);
    let elapsed = t0.elapsed();

    assert_eq!(
        snap.get("agent"),
        Some(&Some(AgentState::Restarting)),
        "snapshot must report the published state"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(50),
        "snapshot blocked {elapsed:?} on a held core.lock — it must read the \
         lock-free published mirror, not take core.lock()"
    );
    holder.join().unwrap();
}

/// #3348 RED: an attached thin client has a pane but no local registry
/// handle, so the render snapshot must not manufacture `Idle` for it.
#[test]
fn thin_client_missing_registry_state_is_explicitly_unknown_3348() {
    let pane = Pane {
        agent_name: "remote".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(38, 11),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: Some(crate::backend::Backend::ClaudeCode),
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    let mut layout = Layout::new();
    layout.add_tab(crate::layout::Tab::new("remote".to_string(), pane));
    let registry: AgentRegistry =
        std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

    let snapshot = build_agent_state_snapshot(&layout, &registry);
    assert_eq!(
        format!("{:?}", snapshot.get("remote")),
        "Some(None)",
        "missing local registry state must remain render-only unknown, not Idle"
    );

    let remote_states = HashMap::from([("remote".to_string(), Some(AgentState::Active))]);
    let snapshot = build_agent_state_snapshot_with_remote(&layout, &registry, Some(&remote_states));
    assert_eq!(snapshot.get("remote"), Some(&Some(AgentState::Active)));
}

/// #freeze-2: the render loop re-arms `dirty` on this when a budget-capped
/// `drain_output` leaves a backlog — so it MUST report a visible pane's queued
/// rx (else the backlog stalls; correctness rule ①). Cross-platform (no PTY).
#[test]
fn active_tab_has_pending_output_reflects_visible_pane_queue() {
    let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let pane = Pane {
        agent_name: "agent".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(38, 11),
        rx,
        id: 1,
        backend: Some(crate::backend::Backend::ClaudeCode),
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    };
    let mut layout = Layout::new();
    layout.add_tab(crate::layout::Tab::new("agent".to_string(), pane));

    // Empty channel → nothing to re-arm.
    assert!(!active_tab_has_pending_output(&layout));
    // Queued output on the visible pane → re-arm so the next frame drains it.
    tx.send(b"backlog".to_vec()).unwrap();
    assert!(active_tab_has_pending_output(&layout));
}

fn pane_with_rx(id: usize, rx: crossbeam_channel::Receiver<Vec<u8>>) -> Pane {
    Pane {
        agent_name: "agent".into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(38, 11),
        rx,
        id,
        backend: Some(crate::backend::Backend::ClaudeCode),
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    }
}

/// #freeze-3 H2 MECHANISM: the per-frame drain re-arm
/// (`active_tab_has_pending_output`, the render loop's signal to keep the
/// VISIBLE catch-up going) only sees the ACTIVE tab. A backgrounded tab's
/// backlog therefore never re-arms a *redraw* — correct, since hidden tabs need
/// no redraw. (Background draining is the job of `drain_all_panes`, gated below;
/// this test pins the re-arm's active-only scope so the two stay decoupled.)
/// Cross-platform.
#[test]
fn rearm_ignores_backgrounded_tab_backlog_freeze3() {
    let (_tx0, rx0) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (tx1, rx1) = crossbeam_channel::unbounded::<Vec<u8>>();
    let mut layout = Layout::new();
    layout.add_tab(crate::layout::Tab::new(
        "t0".to_string(),
        pane_with_rx(1, rx0),
    ));
    layout.add_tab(crate::layout::Tab::new(
        "t1".to_string(),
        pane_with_rx(2, rx1),
    ));
    layout.goto_tab(0); // add_tab focuses the new tab; make t0 the active one.

    // active = 0 (t0). Backlog ONLY on the BACKGROUND tab (t1).
    tx1.send(b"backlog".to_vec()).unwrap();
    assert!(
        !active_tab_has_pending_output(&layout),
        "a backgrounded tab's backlog must NOT re-arm a redraw — its catch-up is \
         invisible; it is drained by `drain_all_panes`, not the redraw re-arm"
    );

    // Switch to t1 → its backlog is now the active tab's → the re-arm fires.
    layout.goto_tab(1);
    assert!(
        active_tab_has_pending_output(&layout),
        "switching to the backlogged tab makes it active → re-arm fires (redraw)"
    );
}

/// #freeze-3 ROOT-FIX GATE: `drain_all_panes` must drain a BACKGROUND tab's
/// pane too (not just the active tab), so a backgrounded busy pane's `rx`
/// converges to EMPTY over a BOUNDED number of frames instead of accumulating
/// unbounded. This is the fix for the residual switch-time catch-up #2385 left:
/// pre-fix only the active tab drained, so switching to a long-backgrounded tab
/// replayed `ceil(backlog / budget)` frames (∝ background duration). RED if
/// `drain_all_panes` skipped background tabs (the bug): the background `rx`
/// would never drain and the loop below would not converge. Cross-platform.
#[test]
fn drain_all_panes_bounds_background_rx_freeze3() {
    const CHUNK: usize = 4 * 1024; // PTY-read-sized chunk

    let (_tx0, rx0) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (tx1, rx1) = crossbeam_channel::unbounded::<Vec<u8>>();
    let mut layout = Layout::new();
    layout.add_tab(crate::layout::Tab::new(
        "t0".to_string(),
        pane_with_rx(1, rx0),
    ));
    layout.add_tab(crate::layout::Tab::new(
        "t1".to_string(),
        pane_with_rx(2, rx1),
    ));
    layout.goto_tab(0); // t0 ACTIVE (idle), t1 BACKGROUND (flooded).

    // Flood the BACKGROUND tab with a backlog the active render path never sees.
    let backlog = 512 * 1024;
    let chunks = backlog / CHUNK; // 128 chunks
    for _ in 0..chunks {
        tx1.send(vec![b'x'; CHUNK]).unwrap();
    }
    let bg_rx_len = |layout: &Layout| -> usize {
        layout.tabs[1]
            .root()
            .find_pane(2)
            .map(|p| p.rx.len())
            .unwrap_or(0)
    };
    assert_eq!(
        bg_rx_len(&layout),
        chunks,
        "precondition: the whole backlog is queued on the background pane"
    );

    // Each `drain_all_panes` call == one render frame. The background pane MUST
    // drain down to empty over a bounded number of frames (the fix), not stay
    // queued (the bug). The active tab is idle and leaves the shared budget, so
    // t1 drains DRAIN_OUTPUT_BUDGET_BYTES (32 KiB = 8 chunks) per frame.
    let mut frames = 0usize;
    let mut prev = bg_rx_len(&layout);
    loop {
        let more = drain_all_panes(&mut layout);
        frames += 1;
        assert!(
            frames < 1_000,
            "background backlog must converge to drained (it did not — the bug: \
             a background tab is never drained, so its rx grows unbounded)"
        );
        let now = bg_rx_len(&layout);
        assert!(
            now < prev,
            "each frame must make progress draining the background pane \
             (prev={prev} now={now})"
        );
        prev = now;
        if !more {
            break;
        }
    }
    assert_eq!(
        bg_rx_len(&layout),
        0,
        "after convergence the background rx is empty → switching to it shows \
         no catch-up"
    );
    assert_eq!(
        frames, 16,
        "512 KiB backlog drains in ceil(512KiB / 32KiB) = 16 bounded frames at \
         the per-pane budget (the backlog itself is now bounded because draining \
         runs every frame for background tabs too)"
    );
}

// ── #freeze-4 restart-flood boot-phase drain ──────────────────────────

/// Build N tabs, each pane flooded with `chunks_per_pane` × 4 KiB (a restart
/// flood). Senders are returned so the caller keeps them alive (rx stays
/// connected). Tab 0 is active.
fn flooded_layout(
    n: usize,
    chunks_per_pane: usize,
) -> (Layout, Vec<crossbeam_channel::Sender<Vec<u8>>>) {
    const CHUNK: usize = 4 * 1024;
    let mut layout = Layout::new();
    let mut txs = Vec::new();
    for id in 1..=n {
        let (tx, rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        for _ in 0..chunks_per_pane {
            tx.send(vec![b'x'; CHUNK]).unwrap();
        }
        layout.add_tab(crate::layout::Tab::new(
            format!("t{id}"),
            pane_with_rx(id, rx),
        ));
        txs.push(tx);
    }
    layout.goto_tab(0);
    (layout, txs)
}

fn rx_len_of(layout: &Layout, tab_idx: usize, pane_id: usize) -> usize {
    layout.tabs[tab_idx]
        .root()
        .find_pane(pane_id)
        .map(|p| p.rx.len())
        .unwrap_or(0)
}

/// #freeze-4 LOAD-BEARING SAFETY: the per-frame TIME cap MUST stop the boot
/// drain mid-pass so the render loop returns to `select!` to service input every
/// frame — a restart flood can never hard-freeze input regardless of backlog.
/// A `Duration::ZERO` cap must yield after the FIRST pane, leaving later panes
/// UNTOUCHED. RED if the time-cap check is removed (neutered): the pass would
/// drain every pane in one call and the untouched assertion fails. Cross-platform.
#[test]
fn drain_all_panes_until_time_cap_yields_after_bounded_work_freeze4() {
    // 100 × 4 KiB = 400 KiB/pane, larger than the boot per-pane budget so a pane
    // can't be drained to empty "for free" in a single visit.
    let (mut layout, txs) = flooded_layout(3, 100);

    let more = drain_all_panes_until(&mut layout, Duration::ZERO);
    assert!(
        more,
        "a ZERO time-cap with backlog remaining must report more pending"
    );
    // The LAST pane in drain order (tab 2, id 3) must still hold its FULL backlog
    // — the ZERO cap stopped the pass long before reaching it.
    assert_eq!(
        rx_len_of(&layout, 2, 3),
        100,
        "ZERO time-cap must yield before draining every pane (without the cap, \
         all panes drain in one call → input would be starved)"
    );
    drop(txs);
}

/// #freeze-4: given time (a generous cap, as the bounded boot window provides),
/// the boot drain clears the WHOLE restart flood across active + background tabs
/// in a bounded number of frames → after the boot phase no pane carries backlog
/// into interactive use.
#[test]
fn drain_all_panes_until_clears_whole_flood_when_uncapped_freeze4() {
    let (mut layout, txs) = flooded_layout(3, 100);

    let mut frames = 0usize;
    loop {
        let more = drain_all_panes_until(&mut layout, Duration::from_secs(30));
        frames += 1;
        assert!(frames < 1000, "boot catch-up must converge");
        if !more {
            break;
        }
    }
    for (tab_idx, pane_id) in [(0usize, 1usize), (1, 2), (2, 3)] {
        assert_eq!(
            rx_len_of(&layout, tab_idx, pane_id),
            0,
            "every pane's restart backlog must be fully drained in the boot phase \
             (tab {tab_idx} pane {pane_id})"
        );
    }
    drop(txs);
}

// ── dim non-focused panes (version b) ──────────────────────────────────────────────

/// blend-fn + colour-resolution determinism, incl. the confirm-first readability point
/// (a dimmed default-fg cell is a clearly-visible mid-grey, not black).
#[test]
fn dim_blend_and_color_resolution_are_deterministic() {
    // RGB blend toward black at 0.5 = halfway (rounded).
    assert_eq!(
        blend_rgb((204, 204, 204), TERM_BG_RGB, 0.5),
        (102, 102, 102)
    );
    assert_eq!(
        blend_rgb((255, 255, 255), TERM_BG_RGB, 0.5),
        (128, 128, 128)
    );
    assert_eq!(blend_rgb((200, 100, 50), TERM_BG_RGB, 0.5), (100, 50, 25));
    assert_eq!(blend_rgb((10, 20, 30), TERM_BG_RGB, 0.0), (10, 20, 30)); // factor 0 = identity

    // Named/Indexed/Reset → RGB resolution.
    assert_eq!(color_to_rgb(Color::White, true), Some((255, 255, 255)));
    assert_eq!(color_to_rgb(Color::Blue, true), Some((0, 0, 128)));
    assert_eq!(color_to_rgb(Color::Indexed(0), true), Some((0, 0, 0)));
    assert_eq!(
        color_to_rgb(Color::Indexed(15), true),
        Some((255, 255, 255))
    );
    assert_eq!(color_to_rgb(Color::Indexed(196), true), Some((255, 0, 0)));
    assert_eq!(color_to_rgb(Color::Indexed(232), true), Some((8, 8, 8)));
    assert_eq!(
        color_to_rgb(Color::Indexed(255), true),
        Some((238, 238, 238))
    );
    // Reset fg resolves to the assumed default fg (so the dominant text dims); Reset bg
    // is already the background → None (left untouched).
    assert_eq!(color_to_rgb(Color::Reset, true), Some(TERM_FG_RGB));
    assert_eq!(color_to_rgb(Color::Reset, false), None);

    // dim_color end-to-end.
    assert_eq!(
        dim_color(Color::Rgb(200, 100, 50), true),
        Color::Rgb(100, 50, 25)
    );
    assert_eq!(dim_color(Color::Reset, false), Color::Reset); // bg untouched
                                                              // confirm-first: default text dims to a visible mid-grey, NOT black/invisible.
    let dimmed_default = dim_color(Color::Reset, true);
    assert_eq!(dimmed_default, Color::Rgb(102, 102, 102));
    assert_ne!(
        dimmed_default,
        Color::Rgb(0, 0, 0),
        "must stay readable, not vanish"
    );
}

/// Buffer wiring (models a 2-pane layout): `dim_pane_content` blends ONLY the region it
/// is given (the non-focused pane's inner rect), leaving every other cell — i.e. the
/// focused pane, which never receives the call — byte-identical. Mirrors how `render_pane`
/// calls it solely under `!focused`.
#[test]
fn dim_pane_content_blends_only_the_nonfocused_region() {
    use ratatui::buffer::Buffer;
    // Two side-by-side 5×3 pane regions: left = focused (x 0..5), right = non-focused.
    let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
    for y in 0..3u16 {
        for x in 0..10u16 {
            buf[(x, y)].set_fg(Color::White);
            buf[(x, y)].set_bg(Color::Reset);
        }
    }
    // One non-focused cell carries an explicit coloured bg to exercise the bg path.
    buf[(7, 1)].set_bg(Color::Blue);

    let nonfocused = Rect::new(5, 0, 5, 3);
    dim_pane_content(&mut buf, nonfocused);

    // Focused (left) region: untouched.
    for y in 0..3u16 {
        for x in 0..5u16 {
            assert_eq!(
                buf[(x, y)].fg,
                Color::White,
                "focused fg unchanged at {x},{y}"
            );
            assert_eq!(
                buf[(x, y)].bg,
                Color::Reset,
                "focused bg unchanged at {x},{y}"
            );
        }
    }
    // Non-focused (right) region: fg blended toward black; Reset bg left as-is.
    for y in 0..3u16 {
        for x in 5..10u16 {
            assert_eq!(
                buf[(x, y)].fg,
                Color::Rgb(128, 128, 128),
                "non-focused fg blended at {x},{y}"
            );
        }
    }
    assert_eq!(
        buf[(5, 0)].bg,
        Color::Reset,
        "Reset bg stays the terminal background"
    );
    // The explicit Blue bg blended toward black.
    assert_eq!(buf[(7, 1)].bg, Color::Rgb(0, 0, 64), "coloured bg dimmed");
}
