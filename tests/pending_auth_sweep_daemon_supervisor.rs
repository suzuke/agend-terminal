//! Review-repro (scope: daemon-supervisor) — `pending_auth` map leak / no
//! live-agent sweep.
//!
//! FINDING (medium / resource-leak): `pending_auth` entries are inserted when an
//! agent enters AuthError and only ever removed inside `resolve_pending_auth` on
//! a Fire/Cancel verdict — reached only for agents present in this tick's
//! `handles` (live agents). If an agent is deleted/redeployed while it has a
//! pending (Wait) AuthError notify, the per-agent loop never visits it again, so
//! its entry is never Cancelled and leaks forever. Beyond unbounded growth, a
//! same-name redeploy inherits the stale entry: the insert uses
//! `.entry(name).or_insert(...)`, which will NOT overwrite, so a later re-page
//! renders the PREVIOUS instance's `from` state and `pane_tail`.
//!
//! Unlike its siblings, `pending_auth` has NO
//! `.retain(|name,_| live_agents.contains(name))`: `notify_tracks` gets one in
//! `run_loop` (the #1923 G4/G5 sweep) and `process_error_recovery` sweeps
//! `retry_tracks` / `apierror_episodes` / `apierror_nudge_counts` /
//! `last_continue_inject`.
//!
//! METHOD: static_invariant (source-scan), mirroring `tests/core_mutex_invariant.rs`
//! and the sibling `*_gc_*` invariant tests. The leak is slow unbounded growth
//! driven only by the live-`run_loop` infinite tick (private `HashMap` local),
//! so we assert the GC DISCIPLINE is present in prod source: a
//! `pending_auth.retain(...)` (or `pending_auth.remove(` keyed on the live set)
//! that drops entries for deleted/redeployed agents.
//!
//! RED now: supervisor.rs contains ZERO `pending_auth.retain(` and no
//! live-agent-keyed `pending_auth` prune → assertion fails.
//! GREEN after fix: a `pending_auth.retain(|name, _| live_agents.contains(name))`
//! sweep lands alongside the existing `notify_tracks.retain` in `run_loop`.

use std::path::PathBuf;

fn supervisor_prod_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("daemon")
        .join("supervisor.rs");
    let src = std::fs::read_to_string(&path).expect("read src/daemon/supervisor.rs");
    // Slice off the test module so test fixtures can't satisfy the scan.
    let cfg_test = ["#[cfg(", "test)]"].concat();
    match src.find(&cfg_test) {
        Some(i) => src[..i].to_string(),
        None => src,
    }
}

#[test]
#[ignore = "daemon-supervisor pending_auth-leak-no-sweep: red until fix; remove #[ignore] after fix to confirm"]
fn pending_auth_map_is_gc_pruned_for_deleted_agents_daemon_supervisor() {
    let prod = supervisor_prod_src();

    // Anchor: the map exists and is inserted into (the thing that must be GC'd).
    let insert_anchor = ["pending_auth", "\n"].concat();
    assert!(
        prod.contains("pending_auth") && prod.contains(".entry(name.clone())"),
        "pending_auth insert anchor missing — re-point this test (insert_anchor sanity: {})",
        insert_anchor.trim()
    );

    // Sibling sweep that already exists (the discipline we mirror). Its presence
    // is the proof that the codebase has a live-agent set to sweep against.
    assert!(
        prod.contains("notify_tracks.retain("),
        "notify_tracks.retain sweep anchor missing — re-point this test"
    );

    // The fix: a live-agent-keyed prune of pending_auth, mirroring the
    // notify_tracks sweep. Accept either a `.retain(` GC or a live-set-keyed
    // `.remove(` — both drop entries for deleted/redeployed agents.
    let has_retain = prod.contains("pending_auth.retain(");
    assert!(
        has_retain,
        "pending_auth is never swept for deleted/redeployed agents — it grows \
         unbounded (one leaked entry per agent that exits while a Wait AuthError \
         notify is pending) AND a same-name redeploy inherits the stale `from`/\
         `pane_tail` because the insert uses `.entry(name).or_insert(...)`. Add a \
         sweep mirroring the notify_tracks one in run_loop, e.g. \
         `pending_auth.retain(|name, _| live_agents.contains(name));`."
    );
}
