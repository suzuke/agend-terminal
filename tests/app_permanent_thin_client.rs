//! Structural guard for the app's permanent thin-client boundary.

#[test]
fn app_has_no_owned_daemon_or_maintenance_path() {
    let source = include_str!("../src/app/mod.rs");
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
}
