//! Everything a restart settles BEFORE it kills the old instance: the params
//! the replacement is spawned with, and the gate that decides when the kill may
//! proceed.
//!
//! Split out of `mod.rs` (#3414/#3415 branch) to hold that file under the
//! 750-LOC handler bound `tests/file_size_invariant.rs` enforces. Behaviour is
//! unchanged — the items are the same, only their home and visibility moved.

use serde_json::{json, Value};
use std::path::Path;

/// #1625: assemble the SPAWN params for a restart. Tags `layout: same-tab` so
/// the respawned pane returns to the tab the killed pane occupied (recorded
/// on its DELETE) instead of opening a fresh tab. `mode` only toggles backend
/// resume args — placement is identical for resume and fresh restarts — so
/// the hint is applied unconditionally.
pub(super) fn restart_spawn_params(
    name: &str,
    backend_command: &str,
    args: &[String],
    working_directory: Option<&Path>,
    env: &std::collections::HashMap<String, String>,
    mode: &str,
) -> Value {
    let mut spawn_params = json!({
        "name": name,
        "backend": backend_command,
        "args": args.join(" "),
        "working_directory": working_directory.map(|p| p.display().to_string()),
        "env": serde_json::to_value(env).unwrap_or(serde_json::Value::Null),
        "layout": "same-tab",
    });
    if mode == "resume" {
        spawn_params["mode"] = json!("resume");
    } else {
        // fresh restart only: arm the daemon's first-turn self-kick so the
        // respawned (context-lost) instance runs its recovery sequence instead of
        // sitting idle until an operator happens to type (the overnight
        // restart-strands-the-fleet failure). INDEPENDENT flag — the SPAWN handler
        // must NOT derive self-kick from SpawnMode::Fresh, which initial fleet
        // spawns also map to; only THIS restart-fresh path sets it.
        spawn_params["self_kick_on_ready"] = json!(true);
    }
    spawn_params
}

/// Grace ceiling for [`await_unsent_draft_or_grace`]: even while the operator
/// keeps typing, force the restart after this long so a context-full / stuck
/// agent can't be deferred indefinitely. The primary release is the operator
/// submitting (draft clears well before this); the ceiling only bounds the
/// pathological continuous-typing case. Tunable.
pub(super) const RESTART_DRAFT_GRACE: std::time::Duration = std::time::Duration::from_secs(60);
/// Re-check cadence while deferring — silent (no per-poll event / nudge).
const RESTART_DRAFT_POLL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DraftGate {
    Proceed,
    Defer,
}

/// Pure restart-gate decision (unit-tested exhaustively): proceed with the kill
/// iff `force`, there is no live operator draft, or the grace ceiling has
/// elapsed; otherwise keep deferring. Kept pure (no clock / no IO) so the whole
/// decision matrix is deterministic without real sleeps.
pub(super) fn restart_draft_gate(
    force: bool,
    has_live_draft: bool,
    elapsed: std::time::Duration,
    grace: std::time::Duration,
) -> DraftGate {
    if force || !has_live_draft || elapsed >= grace {
        DraftGate::Proceed
    } else {
        DraftGate::Defer
    }
}

/// Block the restart while the operator has unsent keystrokes in `name`'s input
/// line, releasing the instant the draft is submitted/cleared or after
/// [`RESTART_DRAFT_GRACE`]. Emits exactly two log lines (defer-start, proceed) —
/// no per-poll noise. Thread-safety rationale is at the call site.
pub(super) fn await_unsent_draft_or_grace(home: &Path, name: &str, force: bool) {
    if restart_draft_gate(
        force,
        crate::inbox::notify::operator_has_live_draft(home, name),
        std::time::Duration::ZERO,
        RESTART_DRAFT_GRACE,
    ) == DraftGate::Proceed
    {
        return; // fast path: force, or no live draft — no wait, no log.
    }
    tracing::info!(%name, "restart deferred: operator has an unsent draft in the input line");
    let start = std::time::Instant::now();
    while restart_draft_gate(
        force,
        crate::inbox::notify::operator_has_live_draft(home, name),
        start.elapsed(),
        RESTART_DRAFT_GRACE,
    ) == DraftGate::Defer
    {
        std::thread::sleep(RESTART_DRAFT_POLL);
    }
    tracing::info!(
        %name,
        elapsed_ms = start.elapsed().as_millis() as u64,
        "restart proceeding: draft submitted/cleared or grace ceiling reached"
    );
}
