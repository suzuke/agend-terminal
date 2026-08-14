//! #3273 fix 2 — the prevention half, rendered into every agent's instructions.
//!
//! A background job started inside a tool call outlives the shell that started
//! it. When that shell exits, the job reparents to init and nothing agend
//! records can prove it is ours, so it is never reaped automatically (see the
//! `doctor` orphan section, which reports such processes and takes no action).
//!
//! This text is incidence reduction, not containment, and it says so: a shell
//! killed with SIGKILL runs no trap at all. Kept in its own file because
//! `src/lib.rs` re-exports it for the integration test that pins both its
//! content and the fact that `build_instructions_body` emits it.

/// Guidance for cleaning up background jobs, in the exact shape agents receive.
///
/// The ordering matters and is asserted: initialise the pid list, install the
/// trap, and only then launch. A job started before the trap exists is
/// unprotected, and the common `LOAD=$!` one-liner remembers only the last job
/// so every earlier one leaks.
pub fn background_process_guidance() -> &'static str {
    r#"## Background jobs you start in a tool call

A background job outlives the shell that started it. When your tool call ends,
the shell exits and the job reparents to init — it keeps running, and agend
cannot prove it belongs to you, so nothing reaps it for you.

Set up the cleanup BEFORE launching anything:

    LOAD=""
    trap 'test -z "$LOAD" || kill $LOAD 2>/dev/null' EXIT INT TERM
    my-load-generator & LOAD="$LOAD $!"
    my-second-worker  & LOAD="$LOAD $!"

Initialise first, install the trap second, launch third. A job started before
the trap exists is unprotected, and `LOAD=$!` on its own remembers only the last
job, so every earlier one is left behind.

This reduces how often the leak happens; it does not remove it. A shell ended
with SIGKILL runs no trap at all, and nothing a shell installs on itself can
cover that case. If you leave something behind anyway, `agend-terminal doctor`
lists orphaned processes for a human to look at.
"#
}
