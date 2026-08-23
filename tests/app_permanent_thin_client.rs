//! Structural guard for the app's permanent thin-client boundary.

#[test]
fn app_has_no_owned_daemon_or_maintenance_path() {
    let source = include_str!("../src/app/mod.rs");
    let source = &source[..source.find("#[cfg(test)]").unwrap_or(source.len())];
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
