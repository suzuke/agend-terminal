//! t-…-17 C12: per-tick reconciler for the durable reviewer-assignment authority
//! ([`crate::daemon::assignment_authority`]).
//!
//! Every tick (cadence `new(1)` = one 10 s base tick) this converges the store
//! against reality for each active `(repo,branch)`:
//!   - **A10a** terminal restart-repair: CAS-tombstone any record whose stored
//!     `pr_number` is already in the RETAINED terminal-marker set (catches the A7
//!     crash-gap and old-generation replays — I18/I19).
//!   - **A2** row recovery: `durable_enqueue` any `Pending` record (idempotent).
//!   - **A3/A4** nudge + repair: a record classifying `Unengaged` whose FIXED-
//!     interval lease is due gets an append-only row repair (A4) and a best-effort
//!     self-IPC WAKE pointer (A3), emitted OUTSIDE all flocks. `Satisfied` /
//!     `EngagedUnsatisfied` / `EngagedPending` (acked) are TRUE stops — never nudged.
//!   - **A10b** reflection: re-derive `reserved_assignments` on the live `PrState`
//!     (declarative, convergent across restart).
//!
//! ## Lock discipline (I11/I15 — assignment-lock OUTER of pr_state-flock)
//! Every store op called here (`durable_enqueue`, `repair_row`,
//! `tombstone_terminal_matches`, `redrive_reserved`) takes the per-`(repo,branch)`
//! assignment lock INTERNALLY and releases it before returning; they are invoked
//! sequentially (never nested), so a same-process `flock` re-acquisition can never
//! deadlock. `redrive_reserved` alone additionally takes the pr_state flock — INNER
//! of the assignment lock, the mandated order. The WAKE pointers are collected while
//! no lock is held and fired only after `reconcile_all_collect` returns.
//!
//! ## Determinism
//! The tested core ([`reconcile_all_collect`]) takes an INJECTED `now`; the only
//! unmockable clock read is in [`AssignmentReconcileHandler::run`], which forwards a
//! freshly-read `now` to the core. Tests drive the core with a fixed timestamp.

use super::{PerTickHandler, TickContext};
use crate::daemon::assignment_authority as store;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// C12 per-tick handler. `new(1)` runs every tick (~10 s); a larger cadence divides
/// that down. Registered in [`crate::daemon::per_tick::build_default_handlers`].
pub(crate) struct AssignmentReconcileHandler {
    cadence: u64,
    tick: AtomicU64,
}

impl AssignmentReconcileHandler {
    pub(crate) fn new(cadence: u64) -> Self {
        Self {
            cadence: cadence.max(1),
            tick: AtomicU64::new(0),
        }
    }
}

impl PerTickHandler for AssignmentReconcileHandler {
    fn name(&self) -> &'static str {
        "assignment_reconcile"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        let n = self.tick.fetch_add(1, Ordering::Relaxed);
        if self.cadence > 1 && !n.is_multiple_of(self.cadence) {
            return;
        }
        // The ONLY unmockable clock read; the tested core takes `now` injected.
        let now = chrono::Utc::now().to_rfc3339();
        reconcile_all(ctx.home, &now);
    }
}

/// Reconcile every active branch, then fire the collected A3 WAKE pointers OUTSIDE
/// all flocks (self-IPC nudge; best-effort). Production entry.
pub(crate) fn reconcile_all(home: &Path, now: &str) {
    for target in reconcile_all_collect(home, now) {
        crate::inbox::notify::wake_review_assignment(home, &target);
    }
}

/// Deterministic tested core: reconcile every active branch and RETURN the set of
/// targets to WAKE (A3), so the caller emits the self-IPC pointers lock-free. Takes
/// `now` injected — no unmockable clock inside.
pub(crate) fn reconcile_all_collect(home: &Path, now: &str) -> Vec<String> {
    let mut wakes = Vec::new();
    for (repo, branch) in store::active_branches(home) {
        wakes.extend(reconcile_branch(home, &repo, &branch, now));
    }
    wakes
}

/// One branch. Returns the targets to WAKE (A3). No lock is held across the calls
/// below — each store op locks internally, so they never nest (no re-lock deadlock).
fn reconcile_branch(home: &Path, repo: &str, branch: &str, now: &str) -> Vec<String> {
    // A10a: terminal restart-repair FIRST — tombstoned records are then excluded
    // from the A2/A3/A4 sweep (the `list_active` read below runs after).
    store::tombstone_terminal_matches(home, repo, branch);

    let mut wakes = Vec::new();
    for record in store::list_active(home, repo, branch) {
        // A2: recover a Pending row (idempotent; nonce-dedup handles a crash-window
        // duplicate). Unconditional — even an acked record's row must be durable.
        if record.row == store::RowState::Pending {
            let _ = store::durable_enqueue(home, repo, branch, &record.target, now);
        }
        // Classify against the LIVE pr_state for THIS record's generation (its
        // pr_number). A pr_state for a different generation, or none, ⇒ Unengaged.
        let prstate = crate::daemon::pr_state::load(home, repo, branch)
            .filter(|p| p.pr_number == record.pr_number);
        // A3/A4: ONLY Unengaged is nudge/repair-eligible. `repair_row` self-gates on
        // the FIXED-interval lease (returns false when not due) and advances it, so
        // two ticks in one interval fire at most once (I12) — the wake follows only
        // an actual repair.
        if store::classify_assignment(&record, prstate.as_ref())
            == store::AssignmentEvidence::Unengaged
            && store::repair_row(home, repo, branch, &record.target, now).unwrap_or(false)
        {
            wakes.push(record.target.clone());
        }
    }

    // A10b: re-derive reserved_assignments on the live pr_state (assignment-OUTER →
    // pr_state-INNER). Convergent across restart; a no-op when no pr_state exists.
    store::redrive_reserved(home, repo, branch);
    wakes
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::daemon::assignment_authority::{self as store, ActiveAssignment, TerminalKind};
    use crate::daemon::pr_state::{self, MergeState, ReviewClass};
    use crate::mcp::handlers::comms_gates::ReviewAuthor;
    use std::path::{Path, PathBuf};

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "agend-asgn-recon-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn mk(repo: &str, branch: &str, target: &str, pr: u64, created: &str) -> ActiveAssignment {
        ActiveAssignment::new_pending(
            repo,
            branch,
            target,
            pr,
            "lead",
            "t-rev-1",
            ReviewClass::Dual,
            ReviewAuthor::External("octocat".into()),
            "Please review PR",
            None,
            None,
            created,
        )
    }

    /// A non-terminal pr_state at `head` for generation `pr` (drives A10b reserved).
    fn open_prstate(home: &Path, repo: &str, branch: &str, pr: u64, head: &str) {
        let mut s = pr_state::new_for_branch(repo, branch, head, ReviewClass::Dual);
        s.pr_number = pr;
        s.merge_state = MergeState::NotReady;
        pr_state::save(home, &s).unwrap();
    }

    fn reserved_targets(home: &Path, repo: &str, branch: &str) -> Vec<String> {
        pr_state::load(home, repo, branch)
            .map(|s| {
                s.reserved_assignments
                    .iter()
                    .map(|r| r.target.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// T16 (B16) — Unengaged FIXED-interval repair/nudge is BOUNDED: repair fires
    /// only when `now >= next_nudge_at`, two ticks inside one interval fire at most
    /// once, and the lease (persisted in the record) survives a "restart" (a fresh
    /// `reconcile_all_collect` call — the core holds no in-memory lease state).
    #[test]
    fn t16_unengaged_fixed_interval_bounded_survives_restart() {
        let home = tmp_home("t16");
        let rec = mk("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        let n0 = rec.delivery_nonce.clone();

        // Tick 1 @ +1s (lease due at created_at): repair fires ⇒ wake + nonce rotates.
        let wakes = reconcile_all_collect(&home, "2026-07-13T00:00:01Z");
        assert_eq!(
            wakes,
            vec!["reviewer".to_string()],
            "lease-due Unengaged ⇒ wake"
        );
        let n1 = store::get(&home, "o/r", "feat/x", "reviewer")
            .unwrap()
            .delivery_nonce;
        assert_ne!(n0, n1, "repair rotated the nonce");

        // Tick 2 @ +30s (SAME interval): throttled ⇒ NO wake, NO rotation.
        let wakes = reconcile_all_collect(&home, "2026-07-13T00:00:30Z");
        assert!(
            wakes.is_empty(),
            "second tick in the interval ⇒ no repair/wake"
        );
        assert_eq!(
            store::get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .delivery_nonce,
            n1,
            "throttled tick did not rotate"
        );

        // "Restart" = fresh core call; the lease is on-disk. @ +90s (next interval):
        // repair fires again ⇒ ≤1 repair per FIXED_INTERVAL, bound survived restart.
        let wakes = reconcile_all_collect(&home, "2026-07-13T00:01:30Z");
        assert_eq!(
            wakes,
            vec!["reviewer".to_string()],
            "next interval ⇒ wake again"
        );
        assert_ne!(
            store::get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .delivery_nonce,
            n1,
            "next-interval repair rotated again"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T18 (B18/B19 CORE) — a DELAYED old-generation terminal (pr G) tombstones ONLY
    /// G records; a co-resident NEW generation (pr G' on the SAME branch) SURVIVES
    /// and reserves.
    #[test]
    fn t18_delayed_old_terminal_spares_new_generation() {
        let home = tmp_home("t18");
        let g = mk("o/r", "feat/x", "rev-g", 30, "2026-07-13T00:00:00Z");
        let gp = mk("o/r", "feat/x", "rev-gp", 31, "2026-07-13T00:00:00Z");
        store::persist(&home, &g).unwrap();
        store::persist(&home, &gp).unwrap();
        open_prstate(&home, "o/r", "feat/x", 31, "sha-31");

        // A DELAYED terminal for the OLD generation G=30 (the scanner's A7 for G).
        store::record_terminal(&home, "o/r", "feat/x", 30, TerminalKind::Merged).unwrap();

        reconcile_all_collect(&home, "2026-07-13T00:05:00Z");

        assert!(
            store::get(&home, "o/r", "feat/x", "rev-g").is_none(),
            "the G=30 record is tombstoned"
        );
        let survivor = store::get(&home, "o/r", "feat/x", "rev-gp").expect("G'=31 survives");
        assert_eq!(survivor.pr_number, 31);
        assert_eq!(
            reserved_targets(&home, "o/r", "feat/x"),
            vec!["rev-gp".to_string()],
            "the surviving new generation reserves (excl the tombstoned old gen)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T19 (B18) — same branch G→G': after G goes terminal, a G' dispatch on the SAME
    /// branch reserves normally; G's retained marker never touches G'.
    #[test]
    fn t19_same_branch_reuse_new_generation_reserves() {
        let home = tmp_home("t19");
        // G=40 terminal first (marker written, no G records left).
        store::record_terminal(&home, "o/r", "feat/x", 40, TerminalKind::Closed).unwrap();
        // G'=41 dispatched on the reused branch.
        store::persist(
            &home,
            &mk("o/r", "feat/x", "reviewer", 41, "2026-07-13T00:00:00Z"),
        )
        .unwrap();
        open_prstate(&home, "o/r", "feat/x", 41, "sha-41");

        reconcile_all_collect(&home, "2026-07-13T00:05:00Z");

        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_some(),
            "G'=41 record is untouched by G=40's marker"
        );
        assert_eq!(
            reserved_targets(&home, "o/r", "feat/x"),
            vec!["reviewer".to_string()],
            "the reused-branch new generation reserves"
        );
        assert!(
            store::terminal_markers(&home, "o/r", "feat/x").contains(40)
                && !store::terminal_markers(&home, "o/r", "feat/x").contains(41),
            "G=40 marked terminal, G'=41 is not"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T20 (B20) — an old-generation replay ALWAYS dies on reconcile, however many
    /// newer generations have since terminated (markers RETAINED, never compacted).
    #[test]
    fn t20_old_generation_replay_always_dies() {
        let home = tmp_home("t20");
        // Ten generations terminate (>8) — markers retained exactly.
        for pr in 50u64..60 {
            store::record_terminal(&home, "o/r", "feat/x", pr, TerminalKind::Merged).unwrap();
        }
        // A stale replay of the OLDEST generation reappears.
        store::persist(
            &home,
            &mk("o/r", "feat/x", "ghost", 50, "2026-07-13T00:00:00Z"),
        )
        .unwrap();
        assert!(store::get(&home, "o/r", "feat/x", "ghost").is_some());

        reconcile_all_collect(&home, "2026-07-13T09:00:00Z");

        assert!(
            store::get(&home, "o/r", "feat/x", "ghost").is_none(),
            "the replayed old-generation record is tombstoned (marker never compacted)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T22 (B18) — crash marker→tombstone: the marker landed but the matching-record
    /// tombstone was MISSED (crash between them); the next reconcile removes it.
    #[test]
    fn t22_crash_marker_then_tombstone_removes_on_reconcile() {
        let home = tmp_home("t22");
        // Marker for G=70 written while no record existed (tombstoned 0) — then the
        // record that SHOULD have been tombstoned appears (the crash-gap leftover).
        store::record_terminal(&home, "o/r", "feat/x", 70, TerminalKind::Merged).unwrap();
        store::persist(
            &home,
            &mk("o/r", "feat/x", "reviewer", 70, "2026-07-13T00:00:00Z"),
        )
        .unwrap();
        assert!(store::get(&home, "o/r", "feat/x", "reviewer").is_some());

        reconcile_all_collect(&home, "2026-07-13T00:05:00Z");

        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "reconcile A10a tombstones the crash-gap record matching a retained marker"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T27 (B10) — reconcile is CONVERGENT + idempotent: two runs with the same
    /// inputs (a crash between store-mut and pr_state-write is a subset of "run
    /// twice") produce the same reserved set and no duplication.
    #[test]
    fn t27_reconcile_convergence_idempotent() {
        let home = tmp_home("t27");
        store::persist(
            &home,
            &mk("o/r", "feat/x", "reviewer", 42, "2026-07-13T00:00:00Z"),
        )
        .unwrap();
        open_prstate(&home, "o/r", "feat/x", 42, "sha-42");

        reconcile_all_collect(&home, "2026-07-13T00:05:00Z");
        let first = reserved_targets(&home, "o/r", "feat/x");
        assert_eq!(first, vec!["reviewer".to_string()]);

        // Second run — idempotent (same reserved set, no dup entries).
        reconcile_all_collect(&home, "2026-07-13T00:05:00Z");
        assert_eq!(
            reserved_targets(&home, "o/r", "feat/x"),
            first,
            "reconcile is convergent — a second run is a no-op on the reserved set"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T34 — DUAL targets on one branch are BOTH reflected in reserved; revoking A
    /// leaves B reserved (per-target CAS, never collapse the branch).
    #[test]
    fn t34_dual_targets_both_reserved_revoke_a_leaves_b() {
        let home = tmp_home("t34");
        store::persist(
            &home,
            &mk("o/r", "feat/x", "rev-a", 21, "2026-07-13T00:00:00Z"),
        )
        .unwrap();
        store::persist(
            &home,
            &mk("o/r", "feat/x", "rev-b", 21, "2026-07-13T00:00:00Z"),
        )
        .unwrap();
        open_prstate(&home, "o/r", "feat/x", 21, "sha-21");

        reconcile_all_collect(&home, "2026-07-13T00:05:00Z");
        let mut both = reserved_targets(&home, "o/r", "feat/x");
        both.sort();
        assert_eq!(
            both,
            vec!["rev-a".to_string(), "rev-b".to_string()],
            "both reserved"
        );

        // Revoke A; reconcile ⇒ only B remains reserved.
        store::revoke(&home, "o/r", "feat/x", "rev-a", "2026-07-13T00:06:00Z").unwrap();
        reconcile_all_collect(&home, "2026-07-13T00:07:00Z");
        assert_eq!(
            reserved_targets(&home, "o/r", "feat/x"),
            vec!["rev-b".to_string()],
            "revoke(A) leaves B reserved"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// T28 — the reconciler handler is registered at cadence `new(1)` and its `run`
    /// actually drives the reconcile when the cadence permits (a lease-due Unengaged
    /// record is repaired). Cross-checked by a source-pin on the registration.
    #[test]
    fn t28_handler_registered_and_runs() {
        assert_eq!(
            AssignmentReconcileHandler::new(1).name(),
            "assignment_reconcile"
        );
        // Source-pin: registered in build_default_handlers at new(1) (~10s).
        let src = std::fs::read_to_string("src/daemon/per_tick/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/daemon/per_tick/mod.rs"))
            .expect("per_tick/mod.rs readable");
        assert!(
            src.contains("AssignmentReconcileHandler::new(1)"),
            "reconciler must be registered at new(1) cadence in build_default_handlers"
        );

        // Functional: run() reconciles when the cadence gate opens (new(1) = always).
        let home = tmp_home("t28");
        // next_nudge_at in the far past ⇒ lease-due against real `now`.
        store::persist(
            &home,
            &mk("o/r", "feat/x", "reviewer", 9, "2020-01-01T00:00:00Z"),
        )
        .unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2020-01-01T00:00:00Z").unwrap();
        let n0 = store::get(&home, "o/r", "feat/x", "reviewer")
            .unwrap()
            .delivery_nonce;

        let registry: crate::agent::AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let externals: crate::agent::ExternalRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs: std::sync::Arc<
            parking_lot::Mutex<std::collections::HashMap<String, crate::daemon::AgentConfig>>,
        > = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        AssignmentReconcileHandler::new(1).run(&ctx);
        assert_ne!(
            store::get(&home, "o/r", "feat/x", "reviewer")
                .unwrap()
                .delivery_nonce,
            n0,
            "the registered handler's run() repaired the lease-due Unengaged record"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
