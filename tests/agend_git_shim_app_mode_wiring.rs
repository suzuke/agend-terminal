//! Verify agend-git-shim init functions run in both daemon and app mode.
//!
//! This test would have caught the original gap where app-mode skipped
//! the 4 init functions (binding::reconcile_hooks, symlink_shim,
//! reconcile_orphans, worktree_pool::reconcile_orphan_leases).

/// Verify the bootstrap prepare path wires all 4 shim init calls + extract.
/// Source-grep approach (same pattern as sprint52_no_self_ipc).
///
/// #1814: the init calls moved out of `prepare`'s textual body into the
/// extracted `resolve_fleet_and_reconcile` helper (which `prepare` calls, and
/// which the successor-handoff path re-uses post-lock). The wiring is still
/// in the prepare path — so this test now (a) confirms `prepare` calls
/// `resolve_fleet_and_reconcile`, and (b) greps that helper's body for the
/// init calls. Net guarantee is unchanged: the shim init runs on the daemon
/// `start` path.
fn fn_body<'a>(src: &'a str, signature: &str) -> &'a str {
    let fn_start = src
        .find(signature)
        .unwrap_or_else(|| panic!("{signature} must exist"));
    let rest = &src[fn_start..];
    let fn_end = rest
        .find("\n/// ")
        .or_else(|| rest.find("\npub fn "))
        .or_else(|| rest.find("\npub(crate) fn "))
        .or_else(|| rest.find("\nfn "))
        .unwrap_or(rest.len());
    &rest[..fn_end]
}

#[test]
fn bootstrap_prepare_contains_all_shim_init_calls() {
    let src = include_str!("../src/bootstrap/mod.rs");

    // (a) prepare must delegate to the extracted helper (the wiring link).
    let prepare_body = fn_body(src, "pub fn prepare(");
    assert!(
        prepare_body.contains("resolve_fleet_and_reconcile("),
        "bootstrap::prepare must call resolve_fleet_and_reconcile (the shim-init home)"
    );

    // (b) the helper must contain all 4 init calls + extract_default.
    let body = fn_body(src, "pub(crate) fn resolve_fleet_and_reconcile(");
    assert!(
        body.contains("binding::reconcile_hooks("),
        "resolve_fleet_and_reconcile must call reconcile_hooks"
    );
    assert!(
        body.contains("binding::symlink_shim("),
        "resolve_fleet_and_reconcile must call symlink_shim"
    );
    assert!(
        body.contains("binding::reconcile_orphans("),
        "resolve_fleet_and_reconcile must call reconcile_orphans"
    );
    assert!(
        body.contains("worktree_pool::reconcile_orphan_leases("),
        "resolve_fleet_and_reconcile must call reconcile_orphan_leases"
    );
    assert!(
        body.contains("protocol::extract_default("),
        "resolve_fleet_and_reconcile must call protocol::extract_default"
    );
}

/// Verify daemon::run does NOT duplicate the init calls (they're in bootstrap now).
///
/// Sprint 63 hotfix (Sprint 21+ bug, surfaced by Sprint 63 W1 cumulative
/// run_core additions): the old `&rest[..rest.len().min(5000)]` slice
/// panics when byte 5000 lands mid-UTF-8-character. The panic was latent
/// because `run_core`'s leading 5000 bytes were all ASCII pre-Sprint-63;
/// Sprint 63 W1 PR-1 #595 + PR-2 #596 + PR-3 #597 + PR-4 #598 cumulative
/// additions (including comments containing Chinese / Unicode chars + new
/// `#587` automation references) crossed the 5000-byte threshold at a
/// non-char-boundary position, triggering the panic on every CI run from
/// 16c30d5 onward.
///
/// Fix: use `rest.chars().take(5000).collect::<String>()` for char-aware
/// truncation. This caps at 5000 *chars* not 5000 *bytes* — slightly
/// looser than the original intent but the assertions only grep for ASCII
/// substrings (`binding::reconcile_hooks(home)`, `binding::symlink_shim(home)`),
/// so any prefix large enough to cover the original 5000 bytes' worth of
/// content is sufficient.
#[test]
fn daemon_run_does_not_duplicate_init_calls() {
    let src = include_str!("../src/daemon/mod.rs");
    let fn_start = src.find("fn run_core(").expect("run_core must exist");
    let rest = &src[fn_start..];
    // Char-aware truncation: avoids panic when byte 5000 lands mid-utf-8-char.
    let check_area: String = rest.chars().take(5000).collect();

    // These should NOT be in daemon::run anymore (moved to bootstrap::prepare).
    assert!(
        !check_area.contains("binding::reconcile_hooks(home)"),
        "daemon::run must not duplicate reconcile_hooks (now in bootstrap)"
    );
    assert!(
        !check_area.contains("binding::symlink_shim(home)"),
        "daemon::run must not duplicate symlink_shim (now in bootstrap)"
    );
}
