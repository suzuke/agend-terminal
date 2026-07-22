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
//!     `EngagedUnsatisfied` is a TRUE stop — never nudged. Generic correlated
//!     ACK state is not code-review evidence.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// C12 per-tick handler. `new(1)` runs every tick (~10 s); a larger cadence divides
/// that down. Registered in [`crate::daemon::per_tick::build_default_handlers`].
pub(crate) struct AssignmentReconcileHandler {
    cadence: u64,
    tick: AtomicU64,
    legacy_census_done: AtomicBool,
}

impl AssignmentReconcileHandler {
    pub(crate) fn new(cadence: u64) -> Self {
        Self {
            cadence: cadence.max(1),
            tick: AtomicU64::new(0),
            legacy_census_done: AtomicBool::new(false),
        }
    }
}

impl PerTickHandler for AssignmentReconcileHandler {
    fn name(&self) -> &'static str {
        "assignment_reconcile"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.legacy_census_done.swap(true, Ordering::AcqRel) {
            log_legacy_cutover_census(ctx.home);
        }
        let n = self.tick.fetch_add(1, Ordering::Relaxed);
        if self.cadence > 1 && !n.is_multiple_of(self.cadence) {
            return;
        }
        // The ONLY unmockable clock read; the tested core takes `now` injected.
        let now = chrono::Utc::now().to_rfc3339();
        reconcile_all(ctx.home, &now);
    }
}

/// One census per daemon process. Enforcement is already fail-closed for these
/// rows; this makes the required operator action visible and enumerates the
/// exact generations that must be re-dispatched during rollout.
fn log_legacy_cutover_census(home: &Path) {
    match store::legacy_active_assignments_strict(home) {
        Ok(rows) => {
            if !rows.is_empty() {
                tracing::error!(
                    count = rows.len(),
                    "task66 cutover: active LegacyAssignments cannot submit code-review receipts; re-dispatch every listed generation"
                );
            }
            for row in rows {
                tracing::warn!(
                    assignment_id = %row.assignment_id,
                    repo = %row.repo,
                    branch = %row.branch,
                    pr_number = row.pr_number,
                    task_id = %row.task_id,
                    target = %row.target,
                    "task66 cutover LegacyAssignment: audited re-dispatch required"
                );
            }
        }
        Err(error) => tracing::error!(
            error = %error,
            "task66 cutover: legacy assignment census unreadable; inventory is UNKNOWN and rollout must stop for repair"
        ),
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
    // Workset = dedup UNION of two `(repo,branch)` identity sources (codex m-…-416):
    //   (a) `store::active_branches` — branches discovered via a PARSEABLE authority
    //       record. It reads the FIRST parseable record to name a branch, so a branch
    //       whose records are ALL corrupt is INVISIBLE here → it would VANISH from the
    //       workset, `redrive_reserved` would never run, `authority_unknown` would stay
    //       stale-false, and the merge gate would stay OPEN on a sole-corrupt record.
    //   (b) `pr_state::list_state_identities` — every LIVE PrState's {repo,branch} read
    //       from file CONTENT (never the lossy filename). This rediscovers an
    //       all-records-corrupt branch via its readable PrState ⇒ `reconcile_branch` →
    //       `redrive_reserved` → probe `Unreadable` ⇒ SET `authority_unknown` (fail
    //       closed). A readable PrState with NO assignments is harmless (probe `Absent`
    //       ⇒ empty derive ⇒ CLEAR — a no-op that keeps the flag false).
    // Dedup via a BTreeSet so a branch in BOTH sources is reconciled exactly once
    // (deterministic order).
    let mut workset = std::collections::BTreeSet::new();
    workset.extend(store::active_branches(home));
    workset.extend(crate::daemon::pr_state::list_state_identities(home));

    let mut wakes = Vec::new();
    for (repo, branch) in workset {
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
    // Load PrState ONCE per branch (shared across all records on this branch).
    let raw_prstate = crate::daemon::pr_state::load(home, repo, branch);
    for record in store::list_active(home, repo, branch) {
        // A2: recover a Pending row (idempotent; nonce-dedup handles a crash-window
        // duplicate). Unconditional — even an acked record's row must be durable.
        if record.row == store::RowState::Pending {
            let _ = store::durable_enqueue(home, repo, branch, &record.target, now);
        }
        // P0: retire assignments whose PrState subject has advanced past the
        // assignment's snapshot. A head or PR-number contradiction means the
        // assignment's review target no longer exists on this branch — nudging it
        // is pointless. Absent/corrupt PrState (load returns None) is not proof
        // of obsolescence, so the assignment is preserved (fail-closed).
        // Head comparison is exact == on String (C3: no prefix, no normalization).
        if let Some(ref state) = raw_prstate {
            let head_contradicts = record
                .reviewed_head
                .as_deref()
                .is_some_and(|rh| rh != state.head_sha);
            let pr_contradicts = state.pr_number != record.pr_number;
            if head_contradicts || pr_contradicts {
                if store::retire_if_id_matches(
                    home,
                    repo,
                    branch,
                    &record.target,
                    record.assignment_id,
                    now,
                )
                .unwrap_or(false)
                {
                    tracing::info!(
                        assignment_id = %record.assignment_id,
                        target = %record.target,
                        repo = %repo,
                        branch = %branch,
                        "assignment retired: PrState contradicts subject"
                    );
                }
                continue;
            }
        }
        // Task-terminal gate (#2878-16): a cancelled/done task cannot produce a
        // valid review — retire the assignment instead of re-nudging it.
        // Fail-closed: route error or unknown task_id → preserve.
        if let Ok(routed) = crate::tasks::load_routed(home, &record.task_id) {
            if matches!(
                routed.task.status,
                crate::task_events::TaskStatus::Cancelled | crate::task_events::TaskStatus::Done
            ) {
                if store::retire_if_id_matches(
                    home,
                    repo,
                    branch,
                    &record.target,
                    record.assignment_id,
                    now,
                )
                .unwrap_or(false)
                {
                    tracing::info!(
                        assignment_id = %record.assignment_id,
                        target = %record.target,
                        task_id = %record.task_id,
                        task_status = %routed.task.status,
                        "assignment retired: owning task is terminal"
                    );
                }
                continue;
            }
        }
        // Classify against the LIVE pr_state for THIS record's generation (its
        // pr_number). A pr_state for a different generation, or none, ⇒ Unengaged.
        let prstate = raw_prstate
            .as_ref()
            .filter(|p| p.pr_number == record.pr_number);
        // A3/A4: ONLY Unengaged is nudge/repair-eligible. `repair_row` self-gates on
        // the FIXED-interval lease (returns false when not due) and advances it, so
        // two ticks in one interval fire at most once (I12). Wake fires on an
        // actionable-unread row (pure wake) or a missing/superseded row (repair).
        if store::classify_assignment(&record, prstate) == store::AssignmentEvidence::Unengaged
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
    use crate::task_events::{InstanceName, TaskEvent, TaskId};
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

    /// Simulate the reviewer having READ the inbox row carrying `nonce` (set
    /// `read_at`), making it NON-actionable. Post-B2, `repair_row` only triggers
    /// the append-only nonce-rotation repair for missing/superseded rows — tests
    /// that exercise that path must first mark the current delivery read (an
    /// unread row takes the pure-wake path instead of rotation).
    fn mark_row_read(home: &Path, target: &str, nonce: &str, ts: &str) {
        let path = crate::inbox::storage::inbox_path_resolved(home, target);
        let content = std::fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut msg: crate::inbox::InboxMessage = serde_json::from_str(line).unwrap();
            if msg.delivery_nonce.as_deref() == Some(nonce) {
                msg.read_at = Some(ts.to_string());
            }
            out.push_str(&serde_json::to_string(&msg).unwrap());
            out.push('\n');
        }
        std::fs::write(&path, out).unwrap();
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
    ///
    /// #2914: repair fires only when the inbox row is genuinely missing — not when
    /// merely read. Simulate loss by removing the inbox file.
    #[test]
    fn t16_unengaged_fixed_interval_bounded_survives_restart() {
        let home = tmp_home("t16");
        let rec = mk("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        let n0 = rec.delivery_nonce.clone();
        // #2914: simulate a genuinely lost message (inbox wiped).
        let inbox_path = crate::inbox::storage::inbox_path_resolved(&home, "reviewer");
        std::fs::remove_file(&inbox_path).ok();

        // Tick 1 @ +1s (lease due at created_at): repair fires ⇒ wake + nonce rotates.
        let wakes = reconcile_all_collect(&home, "2026-07-13T00:00:01Z");
        assert_eq!(
            wakes,
            vec!["reviewer".to_string()],
            "lease-due Unengaged (row missing) ⇒ wake"
        );
        let n1 = store::get(&home, "o/r", "feat/x", "reviewer")
            .unwrap()
            .delivery_nonce;
        assert_ne!(n0, n1, "repair rotated the nonce");

        // Tick 2 @ +30s (SAME interval): throttled ⇒ NO wake, NO rotation.
        // The tick-1 repair re-created the inbox with a fresh row, so nonce is present.
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

        // Simulate another loss so the next-interval repair has something to act on.
        std::fs::remove_file(&inbox_path).ok();

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

    /// B2 (A4 guard) — the FIRST reconcile tick must NOT rotate/supersede a HEALTHY,
    /// still-actionable (unread) delivery row. The row stays intact (no nonce
    /// rotation, no supersede), but a pure WAKE fires so the reviewer notices it.
    #[test]
    fn b2_first_tick_does_not_rotate_healthy_unread_row() {
        let home = tmp_home("b2");
        let rec = mk("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
        store::persist(&home, &rec).unwrap();
        // durable_enqueue makes the row ACTIONABLE (healthy, unread); the record's
        // next_nudge_at == created_at, so the very first tick is lease-due.
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        let n0 = rec.delivery_nonce.clone();
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &n0),
            "row is healthy/actionable pre-tick"
        );

        // One reconcile tick with the lease DUE (Unengaged: no ack, no verdict).
        let wakes = reconcile_all_collect(&home, "2026-07-13T00:00:01Z");

        let after = store::get(&home, "o/r", "feat/x", "reviewer").unwrap();
        assert_eq!(
            after.delivery_nonce, n0,
            "a healthy unread row must NOT be rotated on the first tick"
        );
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &n0),
            "a healthy unread row must NOT be superseded (still actionable)"
        );
        assert_eq!(
            wakes,
            vec!["reviewer".to_string()],
            "actionable-unread row triggers pure wake (no rotation)"
        );
        // The lease still advanced (a full interval away) so the guard is bounded.
        assert_ne!(
            after.next_nudge_at, rec.next_nudge_at,
            "next_nudge_at advanced past created_at even though no repair fired"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B4(a) — a CORRUPT authority record must NOT silently DROP its reservation
    /// from the derived `reserved_assignments` set (which would OPEN the reserved
    /// merge gate). The reserved derivation reads records; pre-fix a corrupt record
    /// is indistinguishable from ABSENT (skipped), so the reconciler's A10b re-derive
    /// clears the reservation → `is_merge_ready` opens on a corrupt authority record.
    /// Fail closed: corruption ⇒ KEEP the existing reservation (do not overwrite).
    #[test]
    fn b4_corrupt_record_keeps_reservation_fail_closed() {
        let home = tmp_home("b4-corrupt-rec");
        // TWO active records on the SAME branch/generation. `rev-a` stays healthy so
        // the branch remains in the reconciler's work set (active_branches); `rev-b`
        // is the one we corrupt — its reservation must NOT be dropped.
        store::persist(
            &home,
            &mk("o/r", "feat/x", "rev-a", 42, "2026-07-13T00:00:00Z"),
        )
        .unwrap();
        store::persist(
            &home,
            &mk("o/r", "feat/x", "rev-b", 42, "2026-07-13T00:00:00Z"),
        )
        .unwrap();
        open_prstate(&home, "o/r", "feat/x", 42, "sha-42");

        // First reconcile derives reservations for both healthy records.
        reconcile_all_collect(&home, "2026-07-13T00:05:00Z");
        let mut both = reserved_targets(&home, "o/r", "feat/x");
        both.sort();
        assert_eq!(
            both,
            vec!["rev-a".to_string(), "rev-b".to_string()],
            "both healthy records reserve"
        );

        // Now CORRUPT rev-b's record file and reconcile again. Its reservation MUST
        // survive (fail closed) — never dropped because the record is unreadable.
        let rpath = store::record_path_for_test(&home, "o/r", "feat/x", "rev-b");
        std::fs::write(&rpath, b"{ not valid assignment json").unwrap();

        reconcile_all_collect(&home, "2026-07-13T00:10:00Z");
        let mut after = reserved_targets(&home, "o/r", "feat/x");
        after.sort();
        assert_eq!(
            after,
            vec!["rev-a".to_string(), "rev-b".to_string()],
            "a corrupt authority record must NOT drop its reservation (gate stays closed)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B4 (codex m-…-416) REAL-ENTRY RED — a branch whose SOLE authority record is
    /// corrupt is INVISIBLE to `active_branches` (which names a branch only via its
    /// FIRST PARSEABLE record), so pre-fix it VANISHED from the reconciler workset,
    /// `redrive_reserved` never ran, `authority_unknown` stayed stale-false, and the
    /// merge gate stayed OPEN. The workset UNION with `pr_state::list_state_identities`
    /// rediscovers the branch via its readable PrState ⇒ probe `Unreadable` ⇒ SET the
    /// flag (fail closed) while KEEPING the existing reservation (never dropped on
    /// corruption). This drives the PRODUCTION entry `reconcile_all_collect` — NOT
    /// `redrive_reserved` directly, which would BYPASS the discovery gap. RED pre-fix:
    /// `reconcile_all_collect` never reaches the branch, so the flag stays false.
    #[test]
    fn b4_reconcile_sets_authority_unknown_on_unreadable_fail_closed() {
        let home = tmp_home("b4-recon-set");
        // Seed the branch's SOLE authority record, then CORRUPT it ⇒ probe reports
        // Unreadable (a corrupt record is never conflated with absence) AND
        // active_branches can no longer discover the branch.
        store::persist(
            &home,
            &mk("o/r", "feat/x", "rev-a", 42, "2026-07-13T00:00:00Z"),
        )
        .unwrap();
        let rpath = store::record_path_for_test(&home, "o/r", "feat/x", "rev-a");
        std::fs::write(&rpath, b"{ not valid assignment json").unwrap();

        // A LIVE, READABLE pr_state with authority_unknown CLEAR and an EXISTING
        // reservation the fail-closed transition must PRESERVE. This readable PrState is
        // now the ONLY way the reconciler can discover the branch (union source (b)).
        let mut s = pr_state::new_for_branch("o/r", "feat/x", "sha-42", ReviewClass::Dual);
        s.pr_number = 42;
        s.merge_state = MergeState::NotReady;
        s.authority_unknown = false;
        s.reserved_assignments = vec![pr_state::ReservedAssignment {
            target: "rev-a".to_string(),
            review_author: ReviewAuthor::External("octocat".into()),
            assignment_id: uuid::Uuid::new_v4(),
        }];
        pr_state::save(&home, &s).unwrap();

        // Drive the PRODUCTION reconciler entry. Pre-fix: active_branches misses the
        // corrupt-only branch and the union does not exist ⇒ the branch is never
        // reconciled ⇒ authority_unknown stays false (gate open).
        reconcile_all_collect(&home, "2026-07-13T00:05:00Z");

        let after = pr_state::load(&home, "o/r", "feat/x").expect("pr_state present");
        assert!(
            after.authority_unknown,
            "reconcile via the PrState-identity union on an UNREADABLE authority must SET authority_unknown (fail closed) — pre-fix the corrupt-only branch was never in the workset"
        );
        assert_eq!(
            after
                .reserved_assignments
                .iter()
                .map(|r| r.target.clone())
                .collect::<Vec<_>>(),
            vec!["rev-a".to_string()],
            "the existing reservation is KEPT on unreadable authority (never dropped on corruption)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// B4 (codex m-…-416) REAL-ENTRY — the reconciler must CLEAR `authority_unknown`
    /// once a transient corruption/lock-failure is REPAIRED, WITHOUT waiting for an
    /// unrelated CI event, THROUGH the production entry `reconcile_all_collect`. Here the
    /// record is VALID (repaired) and the seeded state carries a stale
    /// `authority_unknown = true`: the reconciler MUST clear it. (A repaired branch has a
    /// valid record, so `active_branches` already discovers it — this proves the CLEAR
    /// convergence direction runs via the SAME production path as the SET RED above; it
    /// is not itself gated on the union.)
    #[test]
    fn b4_reconcile_clears_authority_unknown_after_repair_no_ci_event() {
        let home = tmp_home("b4-recon-clear");
        // A VALID (repaired) record ⇒ probe reports Active; the assignment lock is taken
        // and the derive succeeds.
        store::persist(
            &home,
            &mk("o/r", "feat/x", "rev-a", 42, "2026-07-13T00:00:00Z"),
        )
        .unwrap();

        // A LIVE pr_state carrying a STALE authority_unknown=true (set earlier by a drain
        // on a now-repaired corruption). No CI event fires — only the reconciler runs.
        let mut s = pr_state::new_for_branch("o/r", "feat/x", "sha-42", ReviewClass::Dual);
        s.pr_number = 42;
        s.merge_state = MergeState::NotReady;
        s.authority_unknown = true;
        pr_state::save(&home, &s).unwrap();

        // Drive the PRODUCTION reconciler entry (NOT record_ci_result, NOT redrive directly).
        reconcile_all_collect(&home, "2026-07-13T00:05:00Z");

        let after = pr_state::load(&home, "o/r", "feat/x").expect("pr_state present");
        assert!(
            !after.authority_unknown,
            "reconcile after repair must CLEAR authority_unknown with NO CI event, through the production entry reconcile_all_collect"
        );
        assert_eq!(
            after
                .reserved_assignments
                .iter()
                .map(|r| r.target.clone())
                .collect::<Vec<_>>(),
            vec!["rev-a".to_string()],
            "the repaired record is derived into the reserved set under the lock"
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
        // #2914: simulate a genuinely lost message so repair fires.
        let inbox_path = crate::inbox::storage::inbox_path_resolved(&home, "reviewer");
        std::fs::remove_file(&inbox_path).ok();

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

    // ─── P0: obsolete review assignment retirement tests ───────────────

    /// Create an assignment with an explicit `reviewed_head`. The standard `mk`
    /// creates a legacy row (reviewed_head = None); these tests exercise the
    /// head-advance retirement path which requires a non-None reviewed_head.
    fn mk_with_head(
        repo: &str,
        branch: &str,
        target: &str,
        pr: u64,
        head: &str,
        created: &str,
    ) -> ActiveAssignment {
        let mut rec = mk(repo, branch, target, pr, created);
        rec.reviewed_head = Some(head.to_string());
        rec
    }

    /// P0-1: PrState head has advanced past the assignment's reviewed_head and no
    /// receipt exists. The assignment is obsolete and must be retired (removed from
    /// the authority store), with no subsequent nudge.
    #[test]
    fn head_advance_no_receipt_retires() {
        let home = tmp_home("p0-1");
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "sha-old",
            "2026-07-13T00:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        // PrState head has advanced to "sha-new" (assignment was for "sha-old").
        open_prstate(&home, "o/r", "feat/x", 7, "sha-new");

        let wakes = reconcile_all_collect(&home, "2026-07-13T00:01:00Z");
        assert!(wakes.is_empty(), "no nudge after retirement");
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "assignment must be retired when PrState head contradicts reviewed_head"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// P0-2: PrState head has advanced and an old REJECTED receipt exists on the
    /// PrState. The head contradiction still retires the assignment — a stale
    /// receipt does not preserve a stale assignment.
    #[test]
    fn head_advance_with_rejected_receipt_retires() {
        let home = tmp_home("p0-2");
        let instance_id = crate::types::InstanceId::new();
        let reviewed_head = "a".repeat(40); // full 40-hex SHA
        let rec = store::ActiveAssignment::new_pending_typed(
            "o/r",
            "feat/x",
            "reviewer",
            instance_id,
            7,
            &reviewed_head,
            crate::review_receipt::ReviewSlot::Primary,
            "lead",
            "t-rev-1",
            ReviewClass::Dual,
            ReviewAuthor::External("octocat".into()),
            "Please review PR",
            None,
            None,
            "2026-07-13T00:00:00Z",
        );
        let assignment_id = rec.assignment_id;
        store::persist(&home, &rec).unwrap();

        // PrState at a NEW head, with a REJECTED receipt for the OLD head.
        let new_head = "b".repeat(40);
        let mut s = pr_state::new_for_branch("o/r", "feat/x", &new_head, ReviewClass::Dual);
        s.pr_number = 7;
        s.merge_state = MergeState::NotReady;
        s.validated_review_receipts = vec![crate::review_receipt::ReviewReceiptSummary {
            receipt_id: "r-1".into(),
            source_id: "s-1".into(),
            evidence_digest: "c".repeat(64),
            assignment_id,
            reviewer_instance_id: instance_id,
            reviewer_name: "reviewer".into(),
            repo: "o/r".into(),
            pr_number: 7,
            branch: "feat/x".into(),
            task_id: "t-rev-1".into(),
            reviewed_head: reviewed_head.clone(),
            review_class: ReviewClass::Dual,
            slot: crate::review_receipt::ReviewSlot::Primary,
            verdict: crate::review_receipt::ReviewVerdict::Rejected,
        }];
        pr_state::save(&home, &s).unwrap();

        let wakes = reconcile_all_collect(&home, "2026-07-13T00:01:00Z");
        assert!(wakes.is_empty(), "no nudge after retirement");
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "head-advanced assignment retires even with an old REJECTED receipt"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// P0-3: PrState pr_number differs from the assignment's pr_number (a new PR
    /// was opened on the same branch). The assignment is for a dead generation and
    /// must be retired.
    #[test]
    fn pr_number_mismatch_retires() {
        let home = tmp_home("p0-3");
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            5,
            "sha-a",
            "2026-07-13T00:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        // PrState is for a DIFFERENT PR number (new generation on same branch).
        open_prstate(&home, "o/r", "feat/x", 6, "sha-a");

        let wakes = reconcile_all_collect(&home, "2026-07-13T00:01:00Z");
        assert!(
            wakes.is_empty(),
            "no nudge for a dead-generation assignment"
        );
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "assignment must be retired when PrState pr_number contradicts"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// P0-4: PrState head matches the assignment's reviewed_head and the assignment
    /// is Unengaged. Normal nudge behavior must be preserved — no retirement.
    #[test]
    fn same_head_unengaged_still_nudges() {
        let home = tmp_home("p0-4");
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "sha-same",
            "2026-07-13T00:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        // #2914: simulate a genuinely lost message so repair fires.
        let inbox_path = crate::inbox::storage::inbox_path_resolved(&home, "reviewer");
        std::fs::remove_file(&inbox_path).ok();
        // PrState head matches the assignment's reviewed_head — no contradiction.
        open_prstate(&home, "o/r", "feat/x", 7, "sha-same");

        let wakes = reconcile_all_collect(&home, "2026-07-13T00:00:01Z");
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_some(),
            "assignment preserved when head matches"
        );
        assert_eq!(
            wakes,
            vec!["reviewer".to_string()],
            "Unengaged nudge still fires when head matches"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// P0-5: ABA guard — an assignment replaced by a new assignment_id between the
    /// lock-free list_active scan and the CAS retire must be preserved. The retire
    /// is a CAS no-op on the new id.
    #[test]
    fn replacement_assignment_preserved_aba() {
        let home = tmp_home("p0-5");
        // Create assignment B (the replacement) with reviewed_head matching PrState.
        let rec_b = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "sha-new",
            "2026-07-13T00:01:00Z",
        );
        store::persist(&home, &rec_b).unwrap();
        open_prstate(&home, "o/r", "feat/x", 7, "sha-new");

        // A stale CAS retire with a different (old) assignment_id must be a no-op.
        let stale_id = uuid::Uuid::new_v4();
        let retired = store::retire_if_id_matches(
            &home,
            "o/r",
            "feat/x",
            "reviewer",
            stale_id,
            "2026-07-13T00:02:00Z",
        )
        .unwrap();
        assert!(!retired, "CAS no-op: stale id does not remove replacement");
        let got = store::get(&home, "o/r", "feat/x", "reviewer").unwrap();
        assert_eq!(
            got.assignment_id, rec_b.assignment_id,
            "replacement assignment preserved with its own id"
        );

        // Reconcile also leaves B alone (head matches).
        reconcile_all_collect(&home, "2026-07-13T00:03:00Z");
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_some(),
            "reconcile preserves replacement assignment whose head matches PrState"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// P0-6: no PrState file exists (never observed by the scanner). The assignment
    /// must be preserved — absent PrState is not proof of obsolescence.
    #[test]
    fn absent_prstate_no_retire() {
        let home = tmp_home("p0-6");
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "sha-old",
            "2026-07-13T00:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        // NO PrState file — the branch was never observed.

        reconcile_all_collect(&home, "2026-07-13T00:01:00Z");
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_some(),
            "assignment preserved when no PrState exists"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// C1: a corrupt (unparseable) PrState file is treated as absent by
    /// `pr_state::load` (returns None). The assignment must be preserved — a
    /// corrupt PrState is not proof of obsolescence (fail-closed).
    #[test]
    fn c1_corrupt_prstate_preserves_assignment() {
        let home = tmp_home("p0-c1");
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "sha-old",
            "2026-07-13T00:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        // Write a CORRUPT PrState file.
        let ps_dir = pr_state::pr_state_dir(&home);
        std::fs::create_dir_all(&ps_dir).unwrap();
        std::fs::write(
            ps_dir.join(pr_state::pr_state_filename("o/r", "feat/x")),
            b"{ not valid prstate json",
        )
        .unwrap();

        reconcile_all_collect(&home, "2026-07-13T00:01:00Z");
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_some(),
            "corrupt PrState must not cause retirement (fail-closed)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// C2: PrState head matches the assignment's reviewed_head and the assignment
    /// is EngagedUnsatisfied (a REJECTED receipt exists at the current head). The
    /// assignment is neither retired (head matches) nor nudged (EngagedUnsatisfied
    /// is a TRUE stop). This is a GREEN test — EngagedUnsatisfied non-nudge behavior
    /// is already implemented; this test pins it in the retirement context.
    #[test]
    fn c2_current_head_engaged_unsatisfied_not_retired() {
        let home = tmp_home("p0-c2");
        let instance_id = crate::types::InstanceId::new();
        let head = "a".repeat(40);
        let rec = store::ActiveAssignment::new_pending_typed(
            "o/r",
            "feat/x",
            "reviewer",
            instance_id,
            7,
            &head,
            crate::review_receipt::ReviewSlot::Primary,
            "lead",
            "t-rev-1",
            ReviewClass::Dual,
            ReviewAuthor::External("octocat".into()),
            "Please review PR",
            None,
            None,
            "2026-07-13T00:00:00Z",
        );
        let assignment_id = rec.assignment_id;
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();

        // PrState at the SAME head with a REJECTED receipt — EngagedUnsatisfied.
        let mut s = pr_state::new_for_branch("o/r", "feat/x", &head, ReviewClass::Dual);
        s.pr_number = 7;
        s.merge_state = MergeState::NotReady;
        s.validated_review_receipts = vec![crate::review_receipt::ReviewReceiptSummary {
            receipt_id: "r-1".into(),
            source_id: "s-1".into(),
            evidence_digest: "c".repeat(64),
            assignment_id,
            reviewer_instance_id: instance_id,
            reviewer_name: "reviewer".into(),
            repo: "o/r".into(),
            pr_number: 7,
            branch: "feat/x".into(),
            task_id: "t-rev-1".into(),
            reviewed_head: head.clone(),
            review_class: ReviewClass::Dual,
            slot: crate::review_receipt::ReviewSlot::Primary,
            verdict: crate::review_receipt::ReviewVerdict::Rejected,
        }];
        pr_state::save(&home, &s).unwrap();

        let wakes = reconcile_all_collect(&home, "2026-07-13T00:01:00Z");
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_some(),
            "EngagedUnsatisfied at current head must NOT be retired"
        );
        assert!(
            wakes.is_empty(),
            "EngagedUnsatisfied is a TRUE stop — no nudge"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// C3: head comparison must be exact full-SHA byte-for-byte — no prefix matching,
    /// no normalization, no case folding. Two SHAs that share a prefix but differ in
    /// the last byte must trigger retirement.
    #[test]
    fn c3_head_sha_exact_byte_comparison() {
        let home = tmp_home("p0-c3");
        // Assignment reviewed_head differs from PrState head only in the last byte.
        let assignment_head = format!("{}0", "a".repeat(39));
        let prstate_head = format!("{}1", "a".repeat(39));
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            &assignment_head,
            "2026-07-13T00:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        open_prstate(&home, "o/r", "feat/x", 7, &prstate_head);

        reconcile_all_collect(&home, "2026-07-13T00:01:00Z");
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "a single-byte difference in full SHA must trigger retirement (exact comparison)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ─── Durability failpoint tests ───────────────────────────────────

    /// D1: inbox persistence failure preserves authority. If the strict
    /// supersede cannot durably retract the stale inbox row, the authority
    /// record must survive so the reconciler retries on the next tick.
    #[cfg(unix)]
    #[test]
    fn d1_supersede_persistence_failure_preserves_authority() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmp_home("d1-persist-fail");
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "sha-old",
            "2026-07-13T00:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        open_prstate(&home, "o/r", "feat/x", 7, "sha-new");

        // Make the inbox directory read-only so the strict supersede write fails.
        let inbox_dir = home.join("inbox");
        let original_perms = std::fs::metadata(&inbox_dir).unwrap().permissions();
        std::fs::set_permissions(&inbox_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let wakes = reconcile_all_collect(&home, "2026-07-13T00:01:00Z");

        // Restore permissions before assertions (cleanup on panic).
        std::fs::set_permissions(&inbox_dir, original_perms).unwrap();

        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_some(),
            "authority MUST survive when inbox supersede fails (fail-closed)"
        );
        assert!(
            wakes.is_empty(),
            "no nudge — the head contradiction still prevents nudging"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// D2: interruption after durable supersede converges on retry. If the
    /// inbox row is already superseded (from a prior interrupted retire) but
    /// the authority was not deleted, a second retire call must converge:
    /// the idempotent supersede is a no-op, and the authority is removed.
    #[test]
    fn d2_interruption_after_supersede_converges_on_retry() {
        let home = tmp_home("d2-converge");
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "sha-old",
            "2026-07-13T00:00:00Z",
        );
        let expected_id = rec.assignment_id;
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();

        // Simulate a prior interrupted retire: supersede the inbox row manually
        // but leave the authority intact.
        let successor = format!("retired-{}", expected_id);
        crate::inbox::storage::supersede_by_nonce_strict(
            &home,
            "reviewer",
            &rec.delivery_nonce,
            &successor,
        )
        .expect("manual supersede must succeed");
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_some(),
            "authority is still present (simulated crash before delete)"
        );

        // Now PrState head advances — reconcile should converge and delete.
        open_prstate(&home, "o/r", "feat/x", 7, "sha-new");
        let wakes = reconcile_all_collect(&home, "2026-07-13T00:02:00Z");

        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "retry after interrupted supersede must converge and delete authority"
        );
        assert!(wakes.is_empty(), "no nudge after retirement");
        std::fs::remove_dir_all(&home).ok();
    }

    /// D3: after retire, the old delivery nonce inbox row is non-actionable
    /// (superseded_by is set or row was already read). Verifies the
    /// supersede-before-delete ordering actually retracts the stale row.
    #[test]
    fn d3_old_nonce_non_actionable_after_retire() {
        let home = tmp_home("d3-nonce");
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "sha-old",
            "2026-07-13T00:00:00Z",
        );
        let old_nonce = rec.delivery_nonce.clone();
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &old_nonce),
            "old nonce must be actionable before retire"
        );

        open_prstate(&home, "o/r", "feat/x", 7, "sha-new");
        reconcile_all_collect(&home, "2026-07-13T00:01:00Z");

        assert!(
            !crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &old_nonce),
            "old nonce must be non-actionable after retire (superseded)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// D4: revocation notice is deterministic and idempotent. When the
    /// reviewer has already read the assignment inbox row, a revocation
    /// notice is enqueued exactly once per retire cycle. A second reconcile
    /// tick (after convergence deletes the authority) enqueues no duplicate.
    #[test]
    fn d4_revocation_notice_idempotent() {
        let home = tmp_home("d4-notice");
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "sha-old",
            "2026-07-13T00:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        // Mark the inbox row as READ so the retire path enqueues a revocation notice.
        mark_row_read(
            &home,
            "reviewer",
            &rec.delivery_nonce,
            "2026-07-13T00:00:30Z",
        );

        open_prstate(&home, "o/r", "feat/x", 7, "sha-new");

        // First reconcile: retires the assignment and enqueues revocation notice.
        reconcile_all_collect(&home, "2026-07-13T00:01:00Z");
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "authority retired on first tick"
        );

        let inbox_content =
            std::fs::read_to_string(home.join("inbox").join("reviewer.jsonl")).unwrap_or_default();
        let notice_count = inbox_content
            .lines()
            .filter(|l| l.contains("review-assignment-revoked"))
            .count();
        assert_eq!(
            notice_count, 1,
            "exactly one revocation notice after first retire"
        );

        // Second reconcile: authority is gone, no duplicate notice.
        reconcile_all_collect(&home, "2026-07-13T00:02:00Z");
        let inbox_content2 =
            std::fs::read_to_string(home.join("inbox").join("reviewer.jsonl")).unwrap_or_default();
        let notice_count2 = inbox_content2
            .lines()
            .filter(|l| l.contains("review-assignment-revoked"))
            .count();
        assert_eq!(
            notice_count2, 1,
            "no duplicate revocation notice after second tick"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// D5: after retire, a stale delivery nonce cannot wake the reviewer.
    /// Even if the nonce somehow survived in a pending state, the reconciler
    /// must not produce a wake for a retired assignment.
    #[test]
    fn d5_retired_assignment_no_stale_wake() {
        let home = tmp_home("d5-no-wake");
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "sha-old",
            "2026-07-13T00:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        open_prstate(&home, "o/r", "feat/x", 7, "sha-new");

        // First tick retires.
        let wakes1 = reconcile_all_collect(&home, "2026-07-13T00:01:00Z");
        assert!(wakes1.is_empty(), "no wake on retirement tick");

        // Second tick: authority gone, no residual wake.
        let wakes2 = reconcile_all_collect(&home, "2026-07-13T00:02:00Z");
        assert!(wakes2.is_empty(), "no stale wake after authority removed");
        std::fs::remove_dir_all(&home).ok();
    }

    /// D6: delete failure after revocation notice enqueue must not duplicate
    /// the notice on retry. The stable nonce + any-state dedup ensures
    /// exactly one notice even across a failed delete + successful retry.
    #[cfg(unix)]
    #[test]
    fn d6_delete_failure_retry_no_duplicate_notice() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmp_home("d6-delete-fail");
        let rec = mk_with_head(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "sha-old",
            "2026-07-13T00:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        mark_row_read(
            &home,
            "reviewer",
            &rec.delivery_nonce,
            "2026-07-13T00:00:30Z",
        );
        open_prstate(&home, "o/r", "feat/x", 7, "sha-new");

        // Make the authority record's parent dir read-only so the delete
        // fails AFTER the notice is enqueued (supersede succeeds because
        // it writes to the inbox dir, not the authority dir).
        // Find the branch dir by scanning reviewer-assignments/.
        let ra_base = home.join("reviewer-assignments");
        let record_dir = std::fs::read_dir(&ra_base)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.is_dir())
            .expect("branch dir must exist after persist");
        let original_perms = std::fs::metadata(&record_dir).unwrap().permissions();
        std::fs::set_permissions(&record_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        // First attempt: notice enqueued, delete fails, authority preserved.
        reconcile_all_collect(&home, "2026-07-13T00:01:00Z");
        std::fs::set_permissions(&record_dir, original_perms.clone()).unwrap();

        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_some(),
            "authority must survive delete failure"
        );
        let inbox_content =
            std::fs::read_to_string(home.join("inbox").join("reviewer.jsonl")).unwrap_or_default();
        let notice_count = inbox_content
            .lines()
            .filter(|l| l.contains("review-assignment-revoked"))
            .count();
        assert_eq!(notice_count, 1, "exactly one notice after first attempt");

        // Second attempt (retry): notice dedup, delete now succeeds.
        reconcile_all_collect(&home, "2026-07-13T00:02:00Z");
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "authority deleted on retry"
        );
        let inbox_content2 =
            std::fs::read_to_string(home.join("inbox").join("reviewer.jsonl")).unwrap_or_default();
        let notice_count2 = inbox_content2
            .lines()
            .filter(|l| l.contains("review-assignment-revoked"))
            .count();
        assert_eq!(
            notice_count2, 1,
            "still exactly one notice after retry (dedup by stable nonce)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// D7: persist replacement with pre-existing stable revocation notice
    /// must not duplicate the notice. Seeds: old authority + already-enqueued
    /// revocation notice (simulating a prior persist that enqueued but whose
    /// atomic_write_json failed). A new persist replacement must see the
    /// existing nonce and skip the duplicate enqueue.
    #[test]
    fn d7_persist_replacement_dedup_revocation_notice() {
        let home = tmp_home("d7-persist-dedup");
        let instance_id = crate::types::InstanceId::new();
        let head = "a".repeat(40);
        let old = store::ActiveAssignment::new_pending_typed(
            "o/r",
            "feat/x",
            "reviewer",
            instance_id,
            7,
            &head,
            crate::review_receipt::ReviewSlot::Primary,
            "lead",
            "t-rev-1",
            ReviewClass::Dual,
            ReviewAuthor::External("octocat".into()),
            "Please review PR",
            None,
            None,
            "2026-07-13T00:00:00Z",
        );
        let old_id = old.assignment_id;
        store::persist(&home, &old).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        mark_row_read(
            &home,
            "reviewer",
            &old.delivery_nonce,
            "2026-07-13T00:00:30Z",
        );

        // Seed: a revocation notice with stable nonce already present
        // (simulating a prior persist that enqueued but failed to write).
        let nonce = format!("revoked-{old_id}");
        let notice = crate::inbox::InboxMessage {
            text: "Reviewer assignment revoked.".to_string(),
            kind: Some("review-assignment-revoked".to_string()),
            timestamp: "2026-07-13T00:01:00Z".to_string(),
            delivery_nonce: Some(nonce.clone()),
            ..Default::default()
        };
        crate::inbox::storage::enqueue(&home, "reviewer", notice).unwrap();

        // Now persist a replacement — should NOT duplicate the notice.
        let new_head = "b".repeat(40);
        let replacement = store::ActiveAssignment::new_pending_typed(
            "o/r",
            "feat/x",
            "reviewer",
            instance_id,
            7,
            &new_head,
            crate::review_receipt::ReviewSlot::Primary,
            "lead",
            "t-rev-2",
            ReviewClass::Dual,
            ReviewAuthor::External("octocat".into()),
            "Please review PR v2",
            None,
            None,
            "2026-07-13T00:02:00Z",
        );
        store::persist(&home, &replacement).unwrap();

        let inbox =
            std::fs::read_to_string(home.join("inbox").join("reviewer.jsonl")).unwrap_or_default();
        let notice_count = inbox
            .lines()
            .filter(|l| l.contains("review-assignment-revoked"))
            .count();
        assert_eq!(
            notice_count, 1,
            "persist replacement must not duplicate revocation notice (stable nonce dedup)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    fn seed_and_cancel_task(home: &Path, task_id: &str) {
        seed_and_cancel_task_on_board(home, task_id, None);
    }

    fn seed_and_cancel_task_on_board(home: &Path, task_id: &str, project: Option<&str>) {
        let tid = TaskId(task_id.into());
        let inst = InstanceName("orchestrator".into());
        let board = match project {
            Some(p) => crate::task_events::board_root(home, p),
            None => home.to_path_buf(),
        };
        crate::task_events::append_batch_at(
            &board,
            &inst,
            vec![
                TaskEvent::Created {
                    task_id: tid.clone(),
                    title: "review task".into(),
                    description: String::new(),
                    priority: "high".into(),
                    owner: None,
                    due_at: None,
                    depends_on: vec![],
                    routed_to: None,
                    branch: None,
                    bind: None,
                    eta_secs: None,
                    tags: vec![],
                    parent_id: None,
                },
                TaskEvent::Cancelled {
                    task_id: tid,
                    by: inst.clone(),
                    reason: "BUSY declined".into(),
                },
            ],
        )
        .unwrap();
    }

    #[test]
    fn cancelled_task_assignment_is_retired_not_nudged() {
        let home = tmp_home("cancel-retire");
        seed_and_cancel_task(&home, "t-cancel-retire-1");

        let rec = ActiveAssignment::new_pending(
            "o/r",
            "feat/x",
            "reviewer",
            7,
            "lead",
            "t-cancel-retire-1",
            ReviewClass::Single,
            ReviewAuthor::External("octocat".into()),
            "Please review",
            None,
            None,
            "2026-07-22T00:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-22T00:00:00Z").unwrap();
        mark_row_read(
            &home,
            "reviewer",
            &rec.delivery_nonce,
            "2026-07-22T00:00:01Z",
        );

        let wakes = reconcile_all_collect(&home, "2026-07-22T00:00:02Z");
        assert!(
            wakes.is_empty(),
            "cancelled task's assignment must be retired, not nudged"
        );
        assert!(
            store::get(&home, "o/r", "feat/x", "reviewer").is_none(),
            "retired assignment must be absent from active store"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cancelled_task_on_project_board_assignment_is_retired() {
        let home = tmp_home("cancel-retire-project");
        seed_and_cancel_task_on_board(&home, "t-cancel-proj-1", Some("Hack_agend-terminal"));

        let rec = ActiveAssignment::new_pending(
            "o/r",
            "feat/y",
            "reviewer-b",
            9,
            "lead",
            "t-cancel-proj-1",
            ReviewClass::Single,
            ReviewAuthor::External("octocat".into()),
            "Please review",
            None,
            None,
            "2026-07-22T01:00:00Z",
        );
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/y", "reviewer-b", "2026-07-22T01:00:00Z")
            .unwrap();
        mark_row_read(
            &home,
            "reviewer-b",
            &rec.delivery_nonce,
            "2026-07-22T01:00:01Z",
        );

        let wakes = reconcile_all_collect(&home, "2026-07-22T01:00:02Z");
        assert!(
            wakes.is_empty(),
            "cancelled task on project board must retire its assignment"
        );
        assert!(
            store::get(&home, "o/r", "feat/y", "reviewer-b").is_none(),
            "retired assignment must be absent from active store"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Actionable-unread row: lease-due reconciliation fires exactly one wake
    /// without nonce rotation or supersede. A second tick within 60s is bounded.
    #[test]
    fn actionable_unread_row_triggers_bounded_wake() {
        let home = tmp_home("unread-wake");
        let rec = mk("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
        let nonce_0 = rec.delivery_nonce.clone();
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();

        // Row is unread/actionable. Tick at lease-due time → should wake.
        let wakes = reconcile_all_collect(&home, "2026-07-13T00:00:01Z");
        assert_eq!(
            wakes,
            vec!["reviewer".to_string()],
            "actionable-unread row ⇒ wake"
        );

        // Pure wake: nonce must NOT rotate (no new row, no supersede).
        let nonce_1 = store::get(&home, "o/r", "feat/x", "reviewer")
            .unwrap()
            .delivery_nonce;
        assert_eq!(nonce_0, nonce_1, "pure wake: nonce not rotated");

        // Second tick within 60s → lease not due → no wake.
        let wakes = reconcile_all_collect(&home, "2026-07-13T00:00:30Z");
        assert!(wakes.is_empty(), "second tick within lease ⇒ no wake");

        std::fs::remove_dir_all(&home).ok();
    }

    /// Read or delivering row: lease-due reconciliation does NOT wake.
    #[test]
    fn read_or_delivering_row_does_not_wake() {
        // -- read case --
        let home = tmp_home("read-no-wake");
        let rec = mk("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
        let nonce = rec.delivery_nonce.clone();
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        mark_row_read(&home, "reviewer", &nonce, "2026-07-13T00:00:05Z");
        let wakes = reconcile_all_collect(&home, "2026-07-13T00:00:10Z");
        assert!(wakes.is_empty(), "read row ⇒ no wake");
        std::fs::remove_dir_all(&home).ok();

        // -- delivering case --
        let home = tmp_home("delivering-no-wake");
        let rec = mk("o/r", "feat/x", "reviewer", 7, "2026-07-13T00:00:00Z");
        let nonce = rec.delivery_nonce.clone();
        store::persist(&home, &rec).unwrap();
        store::durable_enqueue(&home, "o/r", "feat/x", "reviewer", "2026-07-13T00:00:00Z").unwrap();
        // drain transitions unread → delivering.
        let drained = crate::inbox::storage::drain(&home, "reviewer");
        assert!(
            drained
                .iter()
                .any(|m| m.delivery_nonce.as_deref() == Some(&nonce)),
            "drained row carries the assignment nonce"
        );
        let wakes = reconcile_all_collect(&home, "2026-07-13T00:00:10Z");
        assert!(wakes.is_empty(), "delivering row ⇒ no wake");
        std::fs::remove_dir_all(&home).ok();
    }
}
