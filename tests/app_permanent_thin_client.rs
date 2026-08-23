//! Structural guard for the app's permanent thin-client boundary.

fn read_rs_tree(root: &std::path::Path) -> String {
    let mut source = String::new();
    for entry in std::fs::read_dir(root).expect("read app source directory") {
        let path = entry.expect("read app source entry").path();
        if path.is_dir() {
            source.push_str(&read_rs_tree(&path));
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            source.push_str(&std::fs::read_to_string(path).expect("read app source file"));
        }
    }
    source
}

#[test]
fn app_has_no_owned_daemon_or_maintenance_path() {
    let source = read_rs_tree(std::path::Path::new("src/app"));
    let forbidden = [
        "Ok(crate::bootstrap::BootstrapOutcome::Owned(",
        "fn start_owned_services(",
        "fn spawn_app_tick(",
        "recv(tick_rx.as_ref()",
        "OwnerRole::Owned",
    ];

    for needle in forbidden {
        assert!(
            !source.contains(needle),
            "app must be a permanent thin client; found owned path `{needle}`"
        );
    }

    let pane_factory = include_str!("../src/app/pane_factory.rs");
    let pane = include_str!("../src/layout/pane.rs");
    let render = include_str!("../src/render/core_render.rs");
    assert!(pane_factory.contains("forwarder_connected.store(false"));
    assert!(pane.contains("if connected.load(Ordering::Acquire)"));
    assert!(render.contains(" [DISCONNECTED]"));
}
