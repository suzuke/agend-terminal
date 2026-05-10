//! Verify agend-git-shim init functions run in both daemon and app mode.
//!
//! This test would have caught the original gap where app-mode skipped
//! the 4 init functions (binding::reconcile_hooks, symlink_shim,
//! reconcile_orphans, worktree_pool::reconcile_orphan_leases).

/// Verify bootstrap::prepare source contains all 4 init calls.
/// Source-grep approach (same pattern as sprint52_no_self_ipc).
#[test]
fn bootstrap_prepare_contains_all_shim_init_calls() {
    let src = include_str!("../src/bootstrap/mod.rs");
    let fn_start = src
        .find("pub fn prepare(")
        .expect("prepare function must exist");
    let rest = &src[fn_start..];
    let fn_end = rest
        .find("\n/// ")
        .or_else(|| rest.find("\npub fn "))
        .unwrap_or(rest.len());
    let body = &rest[..fn_end];

    assert!(
        body.contains("binding::reconcile_hooks("),
        "bootstrap::prepare must call reconcile_hooks"
    );
    assert!(
        body.contains("binding::symlink_shim("),
        "bootstrap::prepare must call symlink_shim"
    );
    assert!(
        body.contains("binding::reconcile_orphans("),
        "bootstrap::prepare must call reconcile_orphans"
    );
    assert!(
        body.contains("worktree_pool::reconcile_orphan_leases("),
        "bootstrap::prepare must call reconcile_orphan_leases"
    );
    assert!(
        body.contains("protocol::extract_default("),
        "bootstrap::prepare must call protocol::extract_default"
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
