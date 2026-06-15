//! Review-repro guard (app-tui batch): the `Overlay::ConfirmClose` handler in
//! `src/app/overlay.rs` must kill the underlying agent of EVERY closed pane —
//! including shell / non-fleet panes whose `fleet_instance_name` is `None`.
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
//! Driving the real close path deterministically requires spawning a real PTY
//! child and a fleet.yaml/working-dir cleanup in a fire-and-forget thread, so
//! this pins the fix as a SOURCE INVARIANT (mirrors the `send_to_registry` /
//! `broadcast_registry` body-scan tests in `src/agent/tests.rs`): the
//! ConfirmClose handler block must reference an agent-name-based kill path
//! (`agent_name` + `kill_agent`), exactly as the scratch-shell overlay arm
//! does via `super::kill_agent(ctx.home, ctx.registry, &name)`.

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

    let references_agent_name = block.contains("agent_name");
    let references_kill = block.contains("kill_agent");

    assert!(
        references_agent_name && references_kill,
        "resource-leak: the ConfirmClose handler must kill EVERY closed pane's \
         underlying agent, including shell / non-fleet panes (fleet_instance_name \
         == None). It currently collects only `fleet_instance_name` into `names`, \
         so a closed shell pane leaks its PTY child + fd for the whole TUI \
         session. Capture each closed pane's `agent_name` and fall back to \
         `super::kill_agent(ctx.home, ctx.registry, &name)` (mirror the \
         scratch-shell overlay arm). Found agent_name={references_agent_name}, \
         kill_agent={references_kill} in the ConfirmClose block."
    );
}
