//! Review-repro guards for the `Overlay::ConfirmClose` lifecycle boundary.
//! Closing layout must reap local shell children, but a fleet pane is only a
//! view onto a daemon-owned instance and must never delete that instance.
//!
//! Bug: the handler collects only `fleet_instance_name` values into `names`
//! and runs `full_delete_instance` per name in a background thread. A pane
//! created via the `[shell] bash` NewTabMenu item goes through
//! `pane_factory::create_pane`, which sets `fleet_instance_name: None`
//! (pane_factory.rs). For such a pane, `names` is empty, so no kill ever runs;
//! `Pane` has no `Drop` impl and the registry still holds the PTY master +
//! child under the pane's UUID. Nothing iterates closed-pane agents, so the
//! orphaned shell child + fd leak for the whole TUI session.
//!
//! The behavioral tests in `overlay.rs` drive the real close entry point with a
//! registry-backed PTY and fleet fixture. These source invariants additionally
//! keep destructive lifecycle APIs out of the TUI handler except for the
//! bounded ConfirmDeleteInstance arm and require the unmanaged path to use the
//! pane's authoritative `instance_id`.

#[test]
fn confirmclose_kills_nonfleet_pane_agent_app_tui() {
    // Parent of this submodule file is src/app/overlay/, so ../overlay.rs is
    // the source file under test.
    let src = include_str!("../overlay.rs");

    let start = src
        .find("Overlay::ConfirmClose { target } => match key.code {")
        .expect("ConfirmClose handler arm must exist in overlay.rs");
    // The very next overlay arm bounds the ConfirmClose block.
    let rel_end = src[start..]
        .find("Overlay::TabList { ref mut selected } => match key.code {")
        .expect("TabList arm must follow ConfirmClose and bound its block");
    let block = &src[start..start + rel_end];

    // Sanity: the buggy block already collects fleet_instance_name; if THAT
    // ever disappears the slice boundaries drifted and the test is meaningless.
    assert!(
        block.contains("fleet_instance_name"),
        "slice sanity: ConfirmClose block should mention fleet_instance_name; \
         boundaries may have drifted — re-locate the arm"
    );

    let references_instance_id = block.contains("instance_id");
    let references_kill = block.contains("kill_unmanaged_agents(");

    assert!(
        references_instance_id && references_kill,
        "resource-leak: the ConfirmClose handler must kill EVERY closed pane's \
         underlying agent, including shell / non-fleet panes (fleet_instance_name \
         == None). Use each pane's authoritative registry `instance_id`; unmanaged \
         shells are absent from fleet.yaml and cannot be resolved by name. Found \
         instance_id={references_instance_id}, \
         batched_kill={references_kill} in the ConfirmClose block."
    );
}

#[test]
fn confirmclose_never_full_deletes_fleet_instance_app_tui() {
    let src = include_str!("../overlay.rs");
    let production = src
        .split("#[cfg(test)]\nmod tests")
        .next()
        .expect("overlay.rs must have a production section");

    let start = production
        .find("Overlay::ConfirmClose { target } => match key.code {")
        .expect("ConfirmClose handler arm must exist in overlay.rs");
    let delete_start = start
        + production[start..]
            .find("Overlay::ConfirmDeleteInstance {")
            .expect("ConfirmDeleteInstance handler arm must follow ConfirmClose");
    let rel_delete_end = production[delete_start..]
        .find("Overlay::TabList { ref mut selected } => match key.code {")
        .expect("TabList arm must follow ConfirmDeleteInstance and bound its block");
    let delete_end = delete_start + rel_delete_end;
    let confirm_close_block = &production[start..delete_start];
    let delete_block = &production[delete_start..delete_end];

    assert!(
        delete_start > start,
        "delete handler must be a separate arm after ConfirmClose"
    );
    assert!(
        confirm_close_block.contains("fleet_instance_name"),
        "slice sanity"
    );

    let mut outside_delete = String::with_capacity(production.len() - delete_block.len());
    outside_delete.push_str(&production[..delete_start]);
    outside_delete.push_str(&production[delete_end..]);
    for forbidden in [
        "full_delete_instance",
        "instance_state::lifecycle",
        "reconcile_after_close",
        "remove_instance",
        "delete_transaction",
        "kill_agent(",
    ] {
        assert!(
            !outside_delete.contains(forbidden),
            "destructive lifecycle bug: the TUI handler must not reference \
             unused `{forbidden}` outside the bounded ConfirmDeleteInstance \
             control surface."
        );
    }

    let required_lifecycle_call =
        "crate::mcp::handlers::instance_state::lifecycle::full_delete_instance_with_runtime(";
    assert_eq!(
        delete_block.matches(required_lifecycle_call).count(),
        1,
        "the bounded delete arm must call the canonical lifecycle helper"
    );
    let delete_without_required_call = delete_block.replace(required_lifecycle_call, "");
    for forbidden in [
        "full_delete_instance",
        "instance_state::lifecycle",
        "reconcile_after_close",
        "remove_instance",
        "delete_transaction",
        "kill_agent(",
    ] {
        assert!(
            !delete_without_required_call.contains(forbidden),
            "destructive lifecycle bug: the bounded delete arm must not use \
             unused `{forbidden}` in addition to the canonical lifecycle call."
        );
    }
    assert!(
        delete_block.matches("instance_state::lifecycle::").count() == 1,
        "the bounded delete arm may reference only its required lifecycle path"
    );
}
