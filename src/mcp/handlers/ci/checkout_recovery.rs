//! #2755 R3/R4 crash-recovery sweep for checkout transactions — extracted from
//! `checkout_txn.rs` to keep both modules under the MCP-handler LOC ceiling. The Journal
//! state machine + `recover_stale` (checkout-start replay) stay in `checkout_txn`; this
//! module is the boot + per-tick recovery DRIVER (`recover_pending_sweep[_prod]`).

use super::checkout_txn::{
    load_typed, quarantine_corrupt, try_acquire_path_lock, txn_root, Journal, JournalLoad, Phase,
};
use std::path::Path;

/// Drive recovery of CRASHED (orphaned) checkout-transaction journals — the ONE shared
/// callable for boot-repair AND a periodic tick (no dedicated worker; the caller sets
/// cadence). Race-safe against a concurrent live checkout:
///
///   1. read the journal (unlocked) for its path + nonce;
///   2. `try_lock` its EXACT normalized path — SKIP if held (an ACTIVE checkout owns it;
///      not crashed) so a live provision is never disturbed;
///   3. RE-READ under the lock and CAS the nonce — SKIP if the record changed (a NEWER
///      checkout generation took over) so its worktree is never deleted;
///   4. `Committed` ⇒ clear the tombstone; a still-BOUND worktree ⇒ adopt (never delete);
///      any other non-Committed record with a real UNBOUND worktree ⇒ `remove` it
///      (respecting backoff) then clear; on remove failure re-arm + deduped `audit`.
///
/// Handles EVERY non-Committed phase (not just `rollback_pending`), incl. the
/// Prepared-with-real-worktree crash window. `try_lock`/`remove`/`audit` are injected so
/// the sweep is unit-testable without live git or flocks. Returns the count resolved.
pub(crate) fn recover_pending_sweep<G>(
    home: &Path,
    now: chrono::DateTime<chrono::Utc>,
    try_lock: impl Fn(&Journal) -> Option<G>,
    remove: impl Fn(&Journal) -> bool,
    mut audit: impl FnMut(&Journal),
) -> usize {
    let Ok(entries) = std::fs::read_dir(txn_root(home)) else {
        return 0; // no transaction area yet ⇒ nothing pending
    };
    let mut resolved = 0;
    for entry in entries.flatten() {
        let Some(mangled) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // (1) Unlocked read for path + nonce. Sibling `<key>.lock` files load as Absent
        // (skipped); a corrupt record is QUARANTINED (retained as intervention authority —
        // #2755 R3), NOT cleared; an unreadable record fails closed (#2755 R4).
        let seen = match load_typed(home, &mangled) {
            JournalLoad::Loaded(j) => j,
            JournalLoad::Unreadable => {
                // #2755 R4 (item 3a): unreadable ≠ absent — authority uncertain; skip this
                // tick (fail closed), NEVER treat as nothing-to-recover.
                tracing::warn!(mangled = %mangled, "checkout-txn journal unreadable — skipping sweep this tick (fail closed)");
                continue;
            }
            JournalLoad::Corrupt => {
                tracing::warn!(mangled = %mangled, "corrupt checkout-txn journal — quarantined for intervention");
                crate::event_log::log(
                    home,
                    "checkout_txn_corrupt_quarantine",
                    "checkout_txn",
                    &format!("corrupt checkout-txn journal quarantined (manual intervention; managed worktree may remain): {mangled}"),
                );
                if !quarantine_corrupt(home, &mangled) {
                    tracing::warn!(mangled = %mangled, "corrupt-journal quarantine rename failed — retained, will retry next tick");
                }
                continue;
            }
            JournalLoad::Absent => continue,
        };
        // (2) Acquire the EXACT path-lock; a held lock ⇒ a live checkout ⇒ skip.
        let Some(_lock) = try_lock(&seen) else {
            continue;
        };
        // (3) Re-read UNDER the lock + nonce CAS.
        let j = match load_typed(home, &mangled) {
            JournalLoad::Loaded(j) if j.nonce == seen.nonce => j,
            JournalLoad::Loaded(_) => continue, // newer generation took over
            JournalLoad::Unreadable => {
                tracing::warn!(mangled = %mangled, "checkout-txn journal unreadable under lock — skipping (fail closed)");
                continue;
            }
            JournalLoad::Corrupt => {
                // Corrupt after the unlocked read (crashed mid-provision) — QUARANTINE +
                // surface, never clear: same recovery-authority retention as arm (1).
                tracing::warn!(mangled = %mangled, "corrupt checkout-txn journal (under lock) — quarantined for intervention");
                crate::event_log::log(
                    home,
                    "checkout_txn_corrupt_quarantine",
                    "checkout_txn",
                    &format!("corrupt checkout-txn journal quarantined under lock (manual intervention): {mangled}"),
                );
                if !quarantine_corrupt(home, &mangled) {
                    tracing::warn!(mangled = %mangled, "corrupt-journal quarantine rename failed under lock — retained, will retry");
                }
                continue;
            }
            JournalLoad::Absent => continue, // cleared concurrently
        };
        // (4) Resolve.
        if j.phase == Phase::Committed {
            Journal::clear(home, &mangled); // completed attempt's tombstone
            continue;
        }
        if !Path::new(&j.worktree_path).exists() {
            Journal::clear(home, &mangled); // crashed with no worktree on disk
            continue;
        }
        if j.rollback_pending && !j.rollback_due(now) {
            continue; // backoff pacing for an already-armed stuck retry
        }
        // #2755 R4 (item 1): NEVER remove a still-BOUND worktree (crash after bind_full,
        // before Committed) — adopt it as committed. An UNCERTAIN binding fails closed.
        match crate::binding::worktree_binding_state(home, Path::new(&j.worktree_path)) {
            crate::binding::WorktreeBindingState::Bound => {
                Journal::clear(home, &mangled); // effectively committed — keep worktree + binding
                continue;
            }
            crate::binding::WorktreeBindingState::Uncertain => {
                tracing::warn!(mangled = %mangled, "worktree binding unreadable — skipping removal (fail closed)");
                continue;
            }
            crate::binding::WorktreeBindingState::Unbound => {}
        }
        if remove(&j) {
            Journal::clear(home, &mangled);
            resolved += 1;
        } else {
            let mut j = j;
            let was_intervention = j.intervention;
            j.arm_rollback(now);
            if j.intervention && !was_intervention {
                audit(&j); // deduped: only on ENTERING intervention
            }
            let _ = j.save(home, &mangled);
        }
    }
    resolved
}

/// Production entry to [`recover_pending_sweep`] — the ONE shared callable invoked from
/// BOTH boot-repair (`bootstrap::boot_hygiene_sweeps`) and the per-tick recovery handler
/// (no dedicated worker). Supplies the real `git worktree remove --force` (run in each
/// journal's recorded source repo) and the operator-visible INTERVENTION audit
/// (`event_log`). Returns the count resolved this pass.
pub(crate) fn recover_pending_sweep_prod(home: &Path) -> usize {
    recover_pending_sweep(
        home,
        chrono::Utc::now(),
        |j| {
            // Non-blocking exact path-lock; None (a live checkout holds it) ⇒ skip.
            let wt = Path::new(&j.worktree_path);
            let mangled = wt.file_name().and_then(|s| s.to_str()).unwrap_or_default();
            try_acquire_path_lock(home, wt, mangled)
        },
        |j| {
            crate::git_helpers::git_bypass(
                Path::new(&j.source_repo),
                &["worktree", "remove", "--force", &j.worktree_path],
            )
            .map(|o| o.status.success())
            .unwrap_or(false)
        },
        |j| {
            crate::event_log::log(
                home,
                "checkout_txn_intervention",
                "checkout_txn",
                &format!(
                    "stuck checkout-worktree rollback entered INTERVENTION after {} attempts: {}",
                    j.attempts, j.worktree_path
                ),
            );
        },
    )
}
