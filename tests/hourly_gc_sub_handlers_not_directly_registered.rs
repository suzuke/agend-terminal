//! #2616-residual invariant (backlog low, 2026-07-04): `GcTickHandler`,
//! `WorkspaceBoundarySweepHandler`, `TmpReviewGcHandler`, and
//! `ReconcileBackupsGcHandler` still `impl PerTickHandler` each (kept —
//! `gc_tick.rs`'s own test suite drives one of them via `PerTickHandler::run`
//! as a convenience one-liner), but MUST NOT be directly registered in
//! `build_default_handlers`: `HourlyGcHandler` already composes all four via
//! its own `due()`/`work()` calls (#2549 W1 / #2616). A direct
//! `Box::new(GcTickHandler::new(..))` registration there would double-execute
//! every due sweep once via `HourlyGcHandler`'s composition and again via its
//! own standalone slot.
//!
//! METHOD: source-text scan of `build_default_handlers`'s function body,
//! matching `tests/src_file_size_invariant.rs`'s style. Checks for the
//! `Box::new(<Handler>` REGISTRATION pattern, not bare name occurrence — a
//! bare-name scan would false-positive on the function's own explanatory
//! comment, which legitimately names all four while describing the #2549 W1
//! collapse. Registration is the only reachable bypass form here (an
//! accidental registration necessarily writes `Box::new(HandlerName::new(...))`
//! literally at this one call site — trait objects can't enter the vec any
//! other way), so the literal scan is sufficient (contrast the #2612 lesson:
//! there the scanned surface had import/alias/re-export bypass forms; here
//! there is exactly one construction site to check).

use std::path::PathBuf;

const SUB_HANDLERS: &[&str] = &[
    "GcTickHandler",
    "WorkspaceBoundarySweepHandler",
    "TmpReviewGcHandler",
    "ReconcileBackupsGcHandler",
];

#[test]
fn hourly_gc_sub_handlers_are_not_directly_registered() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/daemon/per_tick/mod.rs");
    let content = std::fs::read_to_string(&path).expect("src/daemon/per_tick/mod.rs must exist");
    let lines: Vec<&str> = content.lines().collect();

    let start = lines
        .iter()
        .position(|l| l.contains("fn build_default_handlers"))
        .expect("build_default_handlers must exist in per_tick/mod.rs");
    let end = (start..lines.len())
        .find(|&i| i > start && lines[i] == "}")
        .expect("build_default_handlers must have a closing brace");

    let body = lines[start..=end].join("\n");

    for handler in SUB_HANDLERS {
        let pattern = format!("Box::new({handler}");
        assert!(
            !body.contains(&pattern),
            "#2616-residual: `{handler}` must not be directly registered in \
             build_default_handlers (found `{pattern}`) — HourlyGcHandler \
             already composes it via due()/work(); a direct registration \
             would double-execute its sweep every due tick. Drive it through \
             HourlyGcHandler instead."
        );
    }

    // Guards against the invariant trivially passing if the composition
    // wrapper's own registration were ever deleted (the four sweeps would
    // then never run at all).
    assert!(
        body.contains("Box::new(HourlyGcHandler"),
        "#2616-residual: HourlyGcHandler must be registered in \
         build_default_handlers — it's the sole entry point that drives the \
         four GC sub-sweeps"
    );
}
