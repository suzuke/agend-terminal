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
//! keep destructive lifecycle APIs out of the ConfirmClose arm and require the
//! unmanaged path to use the pane's authoritative `instance_id`.

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

    let start = src
        .find("Overlay::ConfirmClose { target } => match key.code {")
        .expect("ConfirmClose handler arm must exist in overlay.rs");
    let delete_start = src
        .find("Overlay::ConfirmDeleteInstance {\n            name,\n            input,\n            notice,")
        .expect("ConfirmDeleteInstance handler arm must follow ConfirmClose");
    let block = &src[start..delete_start];

    assert!(block.contains("fleet_instance_name"), "slice sanity");
    for forbidden in [
        "full_delete_instance",
        "instance_state::lifecycle",
        "reconcile_after_close",
        "remove_instance",
        "delete_transaction",
        "kill_agent(",
    ] {
        assert!(
            !block.contains(forbidden),
            "destructive lifecycle bug: ConfirmClose must not reference \
             `{forbidden}`. Permanent deletion belongs only to the separate \
             ConfirmDeleteInstance control surface."
        );
    }

    assert!(
        delete_start > start,
        "delete handler must be a separate arm after ConfirmClose"
    );
    let delete_block = &src[delete_start..];
    assert!(
        delete_block.contains("full_delete_instance_with_runtime"),
        "the explicit delete arm must call the canonical lifecycle helper"
    );
}
