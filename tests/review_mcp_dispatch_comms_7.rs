//! Verification/reproduction test for the `mcp-dispatch-comms` review batch,
//! finding 7 (info / design).
//!
//! When the dispatch auto-watch_ci arm FAILS (write failure / stale
//! binding), `dispatch_auto_bind_lease_with_source_and_chain`
//! (src/mcp/handlers/dispatch_hook/mod.rs) only logs `tracing::error!` and
//! still returns `Ok(DispatchOutcome)`. The `[ci-ready-for-action]` chain
//! then never fires and the only signal is a daemon error log no agent can
//! observe — the documented cause class of the #920/#925/#928/#929
//! overnight stalls. The suggested fix surfaces the arm failure in the
//! `DispatchOutcome` (e.g. a `watch_armed: false` / `watch_error` field) so
//! the dispatching agent can observe that the chain handoff will not
//! happen.
//!
//! Static-invariant method (source scan): the arm-failure-is-swallowed
//! behaviour can only be made OBSERVABLE by adding a field to the
//! `DispatchOutcome` struct (a structural change — see redesign_note).
//! This pins that fix: `DispatchOutcome` must carry a field that surfaces
//! whether the CI watch was armed / the arm error.
//!
//! RED now: `DispatchOutcome` has only `source_repo_tier`,
//! `auto_created_branch`, `fetch_attempted` — no watch-arm field, so a
//! failed auto-arm is invisible to callers.
//!
//! GREEN after fix: a `watch_armed` / `watch_error` field on
//! `DispatchOutcome` makes the failed arm observable.

use std::path::PathBuf;

/// Extract the `struct DispatchOutcome { ... }` definition body.
fn dispatch_outcome_struct_body() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/mcp/handlers/dispatch_hook/mod.rs");
    let src = std::fs::read_to_string(&p).expect("read dispatch_hook/mod.rs");
    let start = src
        .find("struct DispatchOutcome")
        .expect("struct DispatchOutcome not found");
    let brace = src[start..]
        .find('{')
        .map(|o| start + o)
        .expect("opening brace for DispatchOutcome not found");
    let bytes = src.as_bytes();
    let mut depth = 0i32;
    let mut end = brace;
    for (i, &b) in bytes.iter().enumerate().skip(brace) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    src[brace..=end].to_string()
}

#[test]
#[ignore = "mcp-dispatch-comms F7: red until DispatchOutcome surfaces watch-arm failure; remove #[ignore] after fix"]
fn dispatch_outcome_surfaces_watch_arm_failure_mcp_dispatch_comms() {
    let body = dispatch_outcome_struct_body();

    // A field that lets the dispatching agent observe whether the CI watch
    // was armed (or the arm error). Accept the common field-name shapes.
    let surfaces_watch_arm = body.contains("watch_armed")
        || body.contains("watch_error")
        || body.contains("watch_arm_error")
        || body.contains("ci_watch_armed");

    assert!(
        surfaces_watch_arm,
        "DispatchOutcome does not surface the auto-watch_ci arm result. When the arm fails the \
         dispatch still returns Ok(DispatchOutcome) and the only signal is a daemon error log no \
         agent can observe (the #920/#925/#928/#929 overnight-stall class). Add a \
         `watch_armed: bool` / `watch_error` field so a failed auto-arm is observable to the \
         dispatching agent."
    );
}
