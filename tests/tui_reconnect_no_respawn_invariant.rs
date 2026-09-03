//! The DISCONNECTED badge means the TUI bridge socket dropped; it is not proof
//! that the agent process died. Keep the recovery key away from destructive
//! instance lifecycle paths.

#[test]
fn focused_pane_reconnect_cannot_restart_or_respawn_the_agent() {
    let source = include_str!("../src/app/dispatch.rs");
    let start = source
        .find("fn reconnect_focused_pane(")
        .expect("focused-pane reconnect helper must exist");
    let end = source[start..]
        .find("\nfn paste_image")
        .map(|offset| start + offset)
        .expect("paste helper must follow reconnect helper");
    let reconnect = &source[start..end];

    assert!(reconnect.contains("create_remote_pane"));
    assert!(reconnect.contains("reconnect_or_append_agent_pane"));
    for forbidden in [
        "restart_instance",
        "crash_respawn",
        "respawn_watchdog",
        "spawn_agent",
        "delete_instance",
    ] {
        assert!(
            !reconnect.contains(forbidden),
            "bridge reconnect must not invoke destructive lifecycle path {forbidden}"
        );
    }
}
