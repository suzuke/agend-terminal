//! #3631 Team Organizer overlay integration tests.
//!
//! Kept in a `*_tests.rs` sibling so the parent `overlay.rs` stays under the
//! anti-monolith LOC ceiling. Self-contained: it builds its own ctx/pane
//! fixtures rather than reaching into the parent test module's helpers.

use super::*;
use crate::layout::{Layout, Pane, Tab};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn test_task_channel() -> &'static (
    crossbeam_channel::Sender<crate::app::rpc::TaskRequest>,
    crossbeam_channel::Receiver<crate::app::rpc::TaskRequest>,
) {
    static CHANNEL: std::sync::OnceLock<(
        crossbeam_channel::Sender<crate::app::rpc::TaskRequest>,
        crossbeam_channel::Receiver<crate::app::rpc::TaskRequest>,
    )> = std::sync::OnceLock::new();
    CHANNEL.get_or_init(crossbeam_channel::unbounded)
}

fn org_home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("overlay_org_{}_{}", std::process::id(), tag));
    std::fs::create_dir_all(&home).ok();
    home
}

fn org_pane(id: usize, name: &str, fleet: Option<&str>) -> Pane {
    Pane {
        agent_name: name.into(),
        instance_id: crate::types::InstanceId::new(),
        instance_ref: None,
        vterm: crate::vterm::VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id,
        backend: None,
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: fleet.map(str::to_string),
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: crate::layout::PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    }
}

/// #3631: `:arrange` opens a preview that does NOT touch the layout; Enter
/// commits a single transaction and only then saves the session.
#[test]
fn arrange_command_previews_then_applies_and_saves() {
    let home = org_home("organizer");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  dev-1: {}\n  dev-2: {}\nteams:\n  ops:\n    members: [dev-1, dev-2]\n    orchestrator: dev-1\n",
    )
    .expect("fleet.yaml");

    let mut layout = Layout::new();
    layout.add_tab(Tab::new(
        "scatter-1".into(),
        org_pane(1, "dev-1", Some("dev-1")),
    ));
    layout.add_tab(Tab::new(
        "scatter-2".into(),
        org_pane(2, "dev-2", Some("dev-2")),
    ));
    layout.active = 0;

    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };

    let mut overlay = Overlay::Command {
        input: "arrange all".into(),
        selected: 0,
    };
    handle_key(&mut overlay, press(KeyCode::Enter), &mut ctx);

    let shape_before: Vec<(u64, Vec<usize>)> = ctx
        .layout
        .tabs
        .iter()
        .map(|tab| (tab.id, tab.root().pane_ids()))
        .collect();
    assert!(
        matches!(overlay, Overlay::Organizer { .. }),
        "`:arrange` opens the Organizer preview"
    );
    assert_eq!(
        ctx.layout
            .tabs
            .iter()
            .map(|tab| (tab.id, tab.root().pane_ids()))
            .collect::<Vec<_>>(),
        shape_before,
        "preview must not mutate the layout"
    );
    assert!(
        !home.join("session.json").exists(),
        "preview must not save the session"
    );

    // An unrelated key is inert; only Enter commits.
    handle_key(&mut overlay, press(KeyCode::Down), &mut ctx);
    assert!(
        matches!(overlay, Overlay::Organizer { .. }),
        "non-Enter keys keep the preview open"
    );

    handle_key(&mut overlay, press(KeyCode::Enter), &mut ctx);
    assert!(
        matches!(overlay, Overlay::Organizer { applied: true, .. }),
        "Enter applies the transaction"
    );
    let ops = ctx
        .layout
        .tabs
        .iter()
        .find(|tab| tab.name == "ops")
        .expect("team tab built");
    assert_eq!(ops.root().agent_names(), vec!["dev-1", "dev-2"]);
    assert!(
        home.join("session.json").exists(),
        "session is saved only after a successful commit"
    );

    let _ = reap_workers;
    std::fs::remove_dir_all(home).ok();
}

/// #3631: Esc cancels a preview with zero layout mutation.
#[test]
fn arrange_preview_cancel_changes_nothing() {
    let home = org_home("organizer-cancel");
    let mut layout = Layout::new();
    layout.add_tab(Tab::new(
        "scatter-1".into(),
        org_pane(1, "dev-1", Some("dev-1")),
    ));
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    let before = ctx
        .layout
        .tabs
        .iter()
        .map(|tab| (tab.id, tab.root().pane_ids()))
        .collect::<Vec<_>>();
    let mut overlay = Overlay::Command {
        input: "arrange all".into(),
        selected: 0,
    };
    handle_key(&mut overlay, press(KeyCode::Enter), &mut ctx);
    handle_key(&mut overlay, press(KeyCode::Esc), &mut ctx);
    assert!(matches!(overlay, Overlay::None), "Esc cancels");
    assert_eq!(
        ctx.layout
            .tabs
            .iter()
            .map(|tab| (tab.id, tab.root().pane_ids()))
            .collect::<Vec<_>>(),
        before,
        "cancel must leave the layout byte-identical"
    );
    std::fs::remove_dir_all(home).ok();
}

/// #3631 blocking-4: `:arrange <unknown-team>` must surface a visible error and
/// must NOT silently fall back to arranging the current team.
#[test]
fn arrange_unknown_team_is_a_visible_error() {
    let home = org_home("organizer-unknown");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  dev-1: {}\nteams:\n  ops:\n    members: [dev-1]\n    orchestrator: dev-1\n",
    )
    .expect("fleet.yaml");

    let mut layout = Layout::new();
    layout.add_tab(Tab::new(
        "scatter".into(),
        org_pane(1, "dev-1", Some("dev-1")),
    ));

    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _rx) = crossbeam_channel::unbounded();
    let mut name_counter = HashMap::new();
    let mut reap_workers = Vec::new();
    let mut ctx = OverlayCtx {
        layout: &mut layout,
        registry: &registry,
        home: &home,
        fleet_path: &home,
        wakeup_tx: &wakeup_tx,
        name_counter: &mut name_counter,
        task_rpc_tx: &test_task_channel().0,
        restart_request_tx: None,
        reap_workers: &mut reap_workers,
    };
    let mut overlay = Overlay::Command {
        input: "arrange ghost".into(),
        selected: 0,
    };
    handle_key(&mut overlay, press(KeyCode::Enter), &mut ctx);

    match &overlay {
        Overlay::ReconnectNotice { message } => {
            assert!(
                message.contains("unknown team") && message.contains("ghost"),
                "expected an explicit unknown-team error, got {message:?}"
            );
        }
        other => panic!(
            "expected a ReconnectNotice, got a non-notice overlay: {}",
            matches!(other, Overlay::Organizer { .. })
        ),
    }
    assert!(
        matches!(ctx.layout.tabs[0].name.as_str(), "scatter"),
        "an unknown team must not rearrange the layout"
    );
    std::fs::remove_dir_all(home).ok();
}
