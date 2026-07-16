//! PR-D · D5 — the shared janitor disposition core (spike §2/§3).
//!
//! D1-D4 unified the DECISION: every terminal-detection system delegates to the
//! pure classifier [`crate::worktree::disposition::terminal_disposition`]. D5
//! closes the RCA's "no-man's-land between policies" on the DISPOSITION side:
//! the three worktree-destruction mechanisms (managed release, hard remove,
//! `.trash` archive) now funnel through ONE switch, so "one policy, two
//! schedules" holds for BOTH the decision and its execution.
//!
//! [`dispose`] is that switch. It maps a decided [`Disposition`] to its
//! mechanism:
//! - `Release`/`Archive` mechanisms are shared VERBATIM across callers
//!   (`release_full` / `maybe_remove_candidate`).
//! - `Delete` is PARAMETERIZED by the caller's `delete` remover — the sweep
//!   (`git_ok` + windows-retry + `branch -D`) and GC (`git_bypass` + timeout)
//!   deliberately keep different git-invocation wrappers (lead decision
//!   m-20260703064336281447-62 / d-20260710112849030331-3). Unifying those two
//!   wrappers is a separate behavior change with its own PR — NOT smuggled in
//!   here (D5-Q3 ruling B; same honest-partial line as D3 ruling-A).
//!
//! Each caller keeps its own fail-closed pre/post — the binding lock, re-
//! validation, WIP-preserve (#2672), and the #2550 archive-fallthrough — in the
//! caller (D5-Q1). This module owns only the disposition→mechanism binding, not
//! the tier-specific safety wrapping.

use crate::worktree::disposition::Disposition;
use crate::worktree_pool::{GcCandidate, ReleaseOutcome};
use std::path::Path;

/// The result of executing a [`Disposition`] via [`dispose`].
pub(crate) enum DispositionOutcome {
    /// `Keep` — nothing was touched.
    Kept,
    /// `Release` — the managed `release_full` ran; carries its full outcome (the
    /// auto-release caller needs `released`/`error` for its fail-closed retry).
    Released(ReleaseOutcome),
    /// `Delete` — the caller's remover ran. `Ok(())` = removed; `Err(reason)` =
    /// could not remove, so the caller decides what to do with it (GC hands the
    /// reason to its #2550 archive-fallthrough; the sweep just skips).
    Deleted(Result<(), String>),
    /// `Archive` — the shared `.trash` archive path ran (archive-only, the D4
    /// gc.rs:706 ForceReclaim invariant); carries its `RemovalOutcome`.
    Archived(crate::daemon::retention::worktrees::RemovalOutcome),
}

/// The shared disposition-ladder SWITCH (spike §2). Both cadence tiers funnel
/// their decided `disposition` here; see the module docs for the boundary.
///
/// `agent` is used by the `Release` arm; `candidate` by the `Archive` arm;
/// `delete` by the `Delete` arm — each arm reads only what it needs, so a caller
/// that never reaches an arm passes an inert value (`""` / `None` / `|| Ok(())`).
pub(crate) fn dispose(
    home: &Path,
    disposition: Disposition,
    agent: &str,
    candidate: Option<&GcCandidate>,
    delete: impl FnOnce() -> Result<(), String>,
) -> DispositionOutcome {
    match disposition {
        Disposition::Keep => DispositionOutcome::Kept,
        // Binding present + terminal → the full managed release (WIP-preserve →
        // remove → unbind → branch cleanup; fail-closed on unpreservable WIP,
        // #2672). Shared verbatim.
        Disposition::Release => {
            DispositionOutcome::Released(crate::worktree_pool::release_full(home, agent, false))
        }
        // No binding, confirmed-clean terminal → the caller's dir-remover. The
        // wrapper choice stays with the caller (D5-Q3 ruling B).
        Disposition::Delete => DispositionOutcome::Deleted(delete()),
        // Reclaim-worthy but untrustworthy git state → atomic `.trash` archive.
        // Shared verbatim. Only the GC tier produces `Archive`, and it always
        // carries its candidate; a missing candidate is a caller bug → fail toward
        // NOT destroying (keep + LOUD), never archive a phantom.
        Disposition::Archive => match candidate {
            Some(c) => DispositionOutcome::Archived(
                crate::daemon::retention::worktrees::maybe_remove_candidate(home, c),
            ),
            None => {
                tracing::error!(
                    agent,
                    "janitor::dispose: Archive disposition without a GcCandidate — \
                     keeping (fail-closed); this is a caller bug"
                );
                DispositionOutcome::Kept
            }
        },
    }
}

/// Auto-release variant of the shared Release mechanism. The caller supplies the
/// exact disk-fresh binding fingerprint it evaluated; the release transaction
/// refuses if that generation moved before L→A reacquisition.
pub(crate) fn dispose_release_exact(
    home: &Path,
    agent: &str,
    expected: &crate::binding::BindingFingerprint,
) -> DispositionOutcome {
    DispositionOutcome::Released(crate::worktree_pool::release_full_exact(
        home, agent, expected, None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    // PR-D·D5 equivalence pins: the disposition SWITCH binds each `Disposition` to
    // its mechanism and touches NO other. The mechanisms themselves (`release_full`
    // / `maybe_remove_candidate` / the caller's remover) are locked by their own
    // pin families, and the end-to-end "each tier's disposition == dispose(same
    // signals)" is locked by the GC/sweep/auto caller pins staying green through
    // the D5 refactor; these lock the switch's internal routing directly. The
    // Keep/Delete/Archive-None arms exercised here perform no I/O, so `home` is inert.
    fn inert_home() -> std::path::PathBuf {
        std::path::PathBuf::from("/nonexistent-d5-janitor-test")
    }

    #[test]
    fn dispose_delete_runs_only_the_caller_remover() {
        let called = Cell::new(false);
        let out = dispose(&inert_home(), Disposition::Delete, "", None, || {
            called.set(true);
            Ok(())
        });
        assert!(called.get(), "Delete must invoke the caller's remover");
        assert!(matches!(out, DispositionOutcome::Deleted(Ok(()))));
    }

    #[test]
    fn dispose_delete_forwards_remover_error_verbatim() {
        // GC's #2550 archive-fallthrough keys on this exact reason string.
        let out = dispose(&inert_home(), Disposition::Delete, "", None, || {
            Err("git worktree remove failed: boom".to_string())
        });
        match out {
            DispositionOutcome::Deleted(Err(reason)) => {
                assert_eq!(reason, "git worktree remove failed: boom");
            }
            _ => panic!("Delete must forward the remover's Err verbatim"),
        }
    }

    #[test]
    fn dispose_keep_is_a_noop_and_never_removes() {
        let called = Cell::new(false);
        let out = dispose(&inert_home(), Disposition::Keep, "", None, || {
            called.set(true);
            Ok(())
        });
        assert!(
            !called.get(),
            "Keep must NOT invoke the remover (the fail-closed default)"
        );
        assert!(matches!(out, DispositionOutcome::Kept));
    }

    #[test]
    fn dispose_archive_without_candidate_keeps_fail_closed() {
        // Archive is GC-only + always carries a candidate; a missing one is a caller
        // bug → fail toward NOT destroying (keep), never archive a phantom, and never
        // fall into the Delete remover.
        let called = Cell::new(false);
        let out = dispose(&inert_home(), Disposition::Archive, "agent-x", None, || {
            called.set(true);
            Ok(())
        });
        assert!(!called.get(), "Archive must not invoke the Delete remover");
        assert!(
            matches!(out, DispositionOutcome::Kept),
            "Archive without a candidate → fail-closed Keep"
        );
    }
}
