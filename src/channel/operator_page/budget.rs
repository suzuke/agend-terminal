//! #3480 — the operator-page rate budget: authoritative in memory, snapshotted
//! only for restart continuity.
//!
//! **The bug this replaces.** The pre-#3480 counter was read from
//! `$AGEND_HOME/operator_page_rate.json` on every call with
//! `read_to_string().ok().and_then(parse).unwrap_or_default()`. Both failure
//! modes of that expression are an EMPTY budget, so the one file in the loop that
//! an agent can write became a fail-OPEN control: deleting it, or writing a byte
//! of garbage into it, refilled the hour. The daemon now holds the budget in a
//! process-global map behind a lock and treats the file as a snapshot it writes.
//!
//! **Why that is enough within one process.** Because memory is authoritative,
//! deleting or editing the snapshot can never GRANT a slot to a running daemon.
//! The only thing tampering can do is put the budget into a state the daemon will
//! not vouch for — and that state DENIES.
//!
//! **Initialisation is an OPERATOR act.** The first version of this module
//! treated an ABSENT snapshot at initialisation as a genuinely fresh deployment
//! and started empty. An adversarial verification pass proved that this was a
//! bypass across the restart boundary: the daemon and every agent share one OS
//! user, so an agent could delete the snapshot, cause a daemon restart, and have
//! the new process re-initialise empty — a 3-per-hour cap degraded to
//! 3-per-restart. The snapshot is now laid down by the same operator-gated act
//! that turns paging on (`agend-terminal admin config-set operator_page.enabled
//! true` → [`seed_snapshot`]), and an ABSENT snapshot at initialisation is
//! POISON, exactly like a corrupt one.
//!
//! **What that buys, stated without inflation.** It does NOT make the snapshot
//! tamper-proof: under one shared OS user nothing on disk is, and an agent can
//! still delete the file at any moment. What it buys is that deleting it can only
//! ever DENY — noticed at claim time (missing) or after a restart (absent), the
//! answer is the same refusal, logged with the path — and that the only way back
//! is the operator re-running the enable command. Deletion is therefore
//! DETECTABLE and RECOVERABLE ONLY BY THE OPERATOR. An agent can silence its own
//! pager; it can never refill the hour.
//!
//! The two denial modes are deliberately distinguishable at the API boundary:
//! [`ClaimError::RateLimited`] means "you have used your pages this hour" and
//! carries a retry-after; [`ClaimError::Unavailable`] means "this budget's state
//! cannot be trusted" and names which condition tripped it, so the operator can
//! tell a spent budget from a tampered one. Each transition into the untrusted
//! state is logged ONCE, where it happens — not on every subsequent refusal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::{RATE_LIMIT_PER_HOUR, RATE_WINDOW_SECS};

/// Why the budget refuses to vouch for its own state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Poison {
    /// A snapshot was present at initialisation but did not parse.
    CorruptSnapshot,
    /// No snapshot at initialisation. Either the operator has never enabled
    /// paging for this home, or the snapshot was deleted and the daemon has since
    /// restarted. Both DENY, and both are cleared the same way: the operator
    /// re-runs the enable command, which re-seeds.
    SnapshotAbsent,
    /// The snapshot vanished after THIS process initialised from one — caught at
    /// claim time, so memory still holds the true spent count.
    SnapshotMissing,
    /// The snapshot exists but could not be read.
    SnapshotUnusable,
}

impl Poison {
    /// Operator-facing cause. Surfaced verbatim in the refusal payload, so it has
    /// to name the condition rather than merely say "unavailable".
    fn reason(self) -> &'static str {
        match self {
            Poison::CorruptSnapshot => {
                "the operator-page rate snapshot was corrupt when the daemon read it"
            }
            Poison::SnapshotAbsent => {
                "the operator-page rate snapshot is absent — paging has never been seeded for this home, or the snapshot was deleted before the daemon started"
            }
            Poison::SnapshotMissing => {
                "the operator-page rate snapshot is missing after this daemon initialised from one"
            }
            Poison::SnapshotUnusable => "the operator-page rate snapshot could not be read",
        }
    }

    /// Stable machine-readable cause, surfaced as `cause` alongside the
    /// `budget_unavailable` code so a caller can tell ABSENT from CORRUPT without
    /// matching on prose that is free to change.
    fn code(self) -> &'static str {
        match self {
            Poison::CorruptSnapshot => "snapshot_corrupt",
            Poison::SnapshotAbsent => "snapshot_absent",
            Poison::SnapshotMissing => "snapshot_missing",
            Poison::SnapshotUnusable => "snapshot_unusable",
        }
    }
}

/// Why a claim was refused.
pub(crate) enum ClaimError {
    /// The rolling-hour budget is spent. Ordinary, expected, and retryable.
    RateLimited { retry_after_secs: i64 },
    /// The budget state itself is untrustworthy. NOT a rate cap — see the module
    /// header for why the two must not share a code. `cause` is the stable token,
    /// `reason` the operator-facing sentence.
    Unavailable {
        cause: &'static str,
        reason: &'static str,
    },
}

#[derive(Default)]
struct Budget {
    /// The `$AGEND_HOME` this state was initialised for. A different home means a
    /// different deployment, so the state is rebuilt from that home's snapshot.
    home: PathBuf,
    initialised: bool,
    stamps: BTreeMap<String, Vec<i64>>,
    poison: Option<Poison>,
}

fn budget() -> &'static parking_lot::Mutex<Budget> {
    static BUDGET: OnceLock<parking_lot::Mutex<Budget>> = OnceLock::new();
    BUDGET.get_or_init(|| parking_lot::Mutex::new(Budget::default()))
}

/// Restart-continuity snapshot. Not the authority — see the module header.
pub(crate) fn snapshot_path(home: &Path) -> PathBuf {
    home.join("operator_page_rate.json")
}

fn persist(path: &Path, stamps: &BTreeMap<String, Vec<i64>>) -> anyhow::Result<()> {
    let body = serde_json::to_string_pretty(stamps)?;
    crate::store::atomic_write(path, body.as_bytes())
}

/// Build (or rebuild) the in-memory budget for `home`.
///
/// PRESENT and parses ⇒ load it. PRESENT and corrupt ⇒ poison. ABSENT ⇒ poison
/// too: initialisation does NOT lay a snapshot down. Only the operator does, via
/// [`seed_snapshot`] on the enable command. Starting empty here is what let an
/// agent refill the hour by deleting the file and forcing a restart.
fn ensure_init(state: &mut Budget, home: &Path) {
    if state.initialised && state.home == home {
        return;
    }
    state.home = home.to_path_buf();
    state.initialised = true;
    state.stamps = BTreeMap::new();
    state.poison = None;

    let path = snapshot_path(home);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<BTreeMap<String, Vec<i64>>>(&raw) {
            Ok(stamps) => state.stamps = stamps,
            Err(error) => {
                tracing::error!(
                    snapshot = %path.display(), %error,
                    "operator-page rate snapshot is CORRUPT — budget poisoned, paging denied until the operator repairs or removes it"
                );
                state.poison = Some(Poison::CorruptSnapshot);
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::error!(
                snapshot = %path.display(),
                "operator-page rate snapshot is ABSENT — budget poisoned, paging denied until the operator re-runs `agend-terminal admin config-set operator_page.enabled true` to re-seed it"
            );
            state.poison = Some(Poison::SnapshotAbsent);
        }
        Err(error) => {
            tracing::error!(
                snapshot = %path.display(), %error,
                "operator-page rate snapshot is unreadable — budget poisoned"
            );
            state.poison = Some(Poison::SnapshotUnusable);
        }
    }
}

/// Lay down an EMPTY snapshot for `home` if there is not one already.
///
/// This is the OPERATOR-gated half of the design: it runs from
/// `runtime_config::set("operator_page.enabled", true)`, whose only mutation
/// surface is `agend-terminal admin config-set`. Because initialisation refuses
/// to invent a snapshot, this is the ONLY way a home acquires one — which is what
/// makes a later absence deny instead of refill.
///
/// Two properties matter and are both pinned by tests:
///
///   * It never clobbers. The file is created with `create_new`, so an existing
///     snapshot — spent stamps and all — is left exactly as it is, and an
///     operator re-running the enable command mid-hour does not refund it.
///   * The remedy actually works without a restart. If this process was already
///     poisoned for this home, creating the snapshot clears the poison and writes
///     back what MEMORY holds, not an empty map. So re-seeding after a deletion
///     restores service without handing back the pages already spent.
///
/// The honest limit is the same one the module header states: under one shared OS
/// user this file is not tamper-proof, and re-seeding after a CORRUPT snapshot is
/// removed does start the hour over — but only an operator can do it.
pub(crate) fn seed_snapshot(home: &Path) -> anyhow::Result<()> {
    use std::io::Write;

    let mut state = budget().lock();
    let path = snapshot_path(home);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(b"{}\n")?;
            file.flush()?;
        }
        // Already seeded: leave it alone. This is the no-clobber guarantee.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    if state.initialised && state.home == home {
        state.poison = None;
        let stamps = std::mem::take(&mut state.stamps);
        let outcome = persist(&path, &stamps);
        state.stamps = stamps;
        if let Err(error) = outcome {
            tracing::error!(
                snapshot = %path.display(), %error,
                "operator-page rate snapshot re-seeded but the in-memory count could not be written back"
            );
        }
    }
    tracing::info!(snapshot = %path.display(), "operator-page rate snapshot seeded by the operator");
    Ok(())
}

/// Claim one slot in `orchestrator`'s rolling hour.
///
/// The whole read → prune → decide → write sequence runs under one lock, so two
/// concurrent callers cannot both observe "two spent" and both push a third
/// stamp. `Err` means the page is DROPPED, never queued: the caller is expected to
/// fall back to writing the milestone into SESSION-HANDOFF.md.
pub(crate) fn claim(home: &Path, orchestrator: &str, now: i64) -> Result<usize, ClaimError> {
    let mut state = budget().lock();
    ensure_init(&mut state, home);
    if let Some(poison) = state.poison {
        return Err(ClaimError::Unavailable {
            cause: poison.code(),
            reason: poison.reason(),
        });
    }

    // Initialisation only gets past `poison` when a snapshot was present, so an
    // absent file HERE is a deletion since this process started. Memory is
    // authoritative, so the deletion cannot refill the budget; it can only mark
    // the state untrustworthy, which denies.
    let path = snapshot_path(home);
    if !path.exists() {
        tracing::error!(
            snapshot = %path.display(),
            "operator-page rate snapshot disappeared after initialisation — budget poisoned, paging denied"
        );
        state.poison = Some(Poison::SnapshotMissing);
        return Err(ClaimError::Unavailable {
            cause: Poison::SnapshotMissing.code(),
            reason: Poison::SnapshotMissing.reason(),
        });
    }

    {
        let stamps = state.stamps.entry(orchestrator.to_string()).or_default();
        // Prune only what fell out of the window in the PAST. A stamp dated in the
        // future (a clock jump, or a hand-edited snapshot) is deliberately kept:
        // discarding it is exactly how a skewed clock would buy extra slots.
        stamps.retain(|at| now.saturating_sub(*at) < RATE_WINDOW_SECS);
        if stamps.len() >= RATE_LIMIT_PER_HOUR {
            let oldest = stamps.iter().copied().min().unwrap_or(now);
            let retry_after_secs =
                (RATE_WINDOW_SECS - now.saturating_sub(oldest)).clamp(1, RATE_WINDOW_SECS);
            return Err(ClaimError::RateLimited { retry_after_secs });
        }
        stamps.push(now);
    }
    let remaining = RATE_LIMIT_PER_HOUR.saturating_sub(
        state
            .stamps
            .get(orchestrator)
            .map_or(0, |stamps| stamps.len()),
    );

    if let Err(error) = persist(&path, &state.stamps) {
        // Fail closed: a counter we cannot snapshot is a counter a restart would
        // not see, which is exactly the bypass the snapshot exists to prevent.
        // Roll the claim back so memory keeps describing what actually happened,
        // and refuse — a broken disk denies deterministically rather than letting
        // uncounted pages through.
        drop_stamp(&mut state, orchestrator, now);
        tracing::warn!(%orchestrator, %error, "operator-page rate snapshot unwritable — refusing");
        return Err(ClaimError::Unavailable {
            cause: "snapshot_unwritable",
            reason: "the operator-page rate snapshot could not be written",
        });
    }
    Ok(remaining)
}

/// Give a claimed slot back — used when the fan-out reached ZERO channels, so the
/// page left through no channel at all.
///
/// That is the only case the caller can prove. A nonzero fan-out count means the
/// page reached at least one registered channel, not that it was delivered (see
/// the rollback site in `operator_page.rs`), so a counted page may still have been
/// dropped further down and this function is deliberately not called for it.
pub(crate) fn release(home: &Path, orchestrator: &str, stamp: i64) {
    let mut state = budget().lock();
    ensure_init(&mut state, home);
    if state.poison.is_some() {
        return;
    }
    drop_stamp(&mut state, orchestrator, stamp);
    let path = snapshot_path(home);
    if let Err(error) = persist(&path, &state.stamps) {
        // Memory already reflects the rollback; a failed snapshot write here only
        // means a restart would see the slot as still spent. Denying more than
        // allowed is the safe direction, so this warns rather than escalating.
        tracing::warn!(%orchestrator, %error, "operator-page rate rollback not snapshotted");
    }
}

fn drop_stamp(state: &mut Budget, orchestrator: &str, stamp: i64) {
    if let Some(stamps) = state.stamps.get_mut(orchestrator) {
        if let Some(at) = stamps.iter().rposition(|held| *held == stamp) {
            stamps.remove(at);
        }
    }
}

/// Clear the process-global budget so one test's state cannot leak into the next.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    *budget().lock() = Budget::default();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    // The budget is a process-global. These cases clear it, so they must exclude
    // against every other case that touches it — including the operator-page tool
    // suite, which uses the same unnamed serial group.
    use serial_test::serial;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-operator-page-budget-{}-{tag}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create budget test home");
        dir
    }

    /// A home the operator has never seeded DENIES. Initialisation used to start
    /// empty here, which meant deleting the snapshot and forcing a restart refilled
    /// the hour. The refusal names ABSENT, distinctly from CORRUPT.
    #[test]
    #[serial]
    fn an_unseeded_home_denies_with_the_absent_cause() {
        reset_for_test();
        let home = tmp_home("unseeded");
        assert!(!snapshot_path(&home).exists());
        match claim(&home, "lead", 1_000) {
            Err(ClaimError::Unavailable { cause, .. }) => assert_eq!(cause, "snapshot_absent"),
            _ => panic!("an unseeded home must deny, not start a fresh budget"),
        }
        reset_for_test();
        std::fs::remove_dir_all(&home).ok();
    }

    /// …and the operator's remedy works in-process: seeding clears the poison and
    /// the very next claim succeeds, with no daemon restart needed.
    #[test]
    #[serial]
    fn seeding_clears_the_absent_poison_without_a_restart() {
        reset_for_test();
        let home = tmp_home("seed-clears");
        assert!(claim(&home, "lead", 1_000).is_err());
        seed_snapshot(&home).expect("operator seeds the snapshot");
        assert!(
            claim(&home, "lead", 1_000).is_ok(),
            "re-seeding is the documented remedy — it has to actually restore service"
        );
        reset_for_test();
        std::fs::remove_dir_all(&home).ok();
    }

    /// Seeding must never clobber: an existing snapshot's stamps survive it, so an
    /// operator re-running the enable command mid-hour does not refund the budget.
    #[test]
    #[serial]
    fn seeding_does_not_overwrite_an_existing_snapshot() {
        reset_for_test();
        let home = tmp_home("no-clobber");
        let existing = serde_json::json!({ "lead": [1_000, 1_001] }).to_string();
        std::fs::write(snapshot_path(&home), &existing).expect("write existing snapshot");
        seed_snapshot(&home).expect("seed is a no-op here");
        assert_eq!(
            std::fs::read_to_string(snapshot_path(&home)).expect("snapshot readable"),
            existing,
            "an existing snapshot must be left byte-for-byte alone"
        );
        reset_for_test();
        std::fs::remove_dir_all(&home).ok();
    }

    /// Delete the snapshot and re-seed it inside the SAME process: memory is still
    /// authoritative, so the spent hour is not refunded.
    #[test]
    #[serial]
    fn re_seeding_after_a_deletion_does_not_refund_the_hour() {
        reset_for_test();
        let home = tmp_home("reseed");
        seed_snapshot(&home).expect("operator seeds the snapshot");
        for _ in 0..RATE_LIMIT_PER_HOUR {
            assert!(claim(&home, "lead", 1_000).is_ok());
        }
        std::fs::remove_file(snapshot_path(&home)).expect("delete the snapshot");
        assert!(matches!(
            claim(&home, "lead", 1_000),
            Err(ClaimError::Unavailable { .. })
        ));
        seed_snapshot(&home).expect("operator re-seeds");
        assert!(
            matches!(
                claim(&home, "lead", 1_000),
                Err(ClaimError::RateLimited { .. })
            ),
            "re-seeding restores service but must not hand back spent pages"
        );
        reset_for_test();
        std::fs::remove_dir_all(&home).ok();
    }

    /// The in-memory budget is the authority: rewriting the snapshot with an empty
    /// map does NOT hand back a spent slot.
    #[test]
    #[serial]
    fn editing_the_snapshot_cannot_grant_a_slot() {
        reset_for_test();
        let home = tmp_home("edit");
        seed_snapshot(&home).expect("operator seeds the snapshot");
        for _ in 0..RATE_LIMIT_PER_HOUR {
            assert!(claim(&home, "lead", 1_000).is_ok());
        }
        std::fs::write(snapshot_path(&home), "{}").expect("blank the snapshot");
        assert!(
            matches!(
                claim(&home, "lead", 1_000),
                Err(ClaimError::RateLimited { .. })
            ),
            "memory is authoritative — blanking the file must not refill the hour"
        );
        reset_for_test();
        std::fs::remove_dir_all(&home).ok();
    }

    /// A restart (modelled by clearing the in-memory state) rebuilds the spent
    /// budget from the snapshot rather than starting fresh.
    #[test]
    #[serial]
    fn a_restart_rebuilds_the_spent_budget_from_the_snapshot() {
        reset_for_test();
        let home = tmp_home("restart");
        seed_snapshot(&home).expect("operator seeds the snapshot");
        for _ in 0..RATE_LIMIT_PER_HOUR {
            assert!(claim(&home, "lead", 1_000).is_ok());
        }
        reset_for_test();
        assert!(
            matches!(
                claim(&home, "lead", 1_000),
                Err(ClaimError::RateLimited { .. })
            ),
            "the snapshot must survive a restart"
        );
        reset_for_test();
        std::fs::remove_dir_all(&home).ok();
    }

    /// A corrupt snapshot is poison, not an empty budget — and it is reported as
    /// `Unavailable`, never as an ordinary cap.
    #[test]
    #[serial]
    fn a_corrupt_snapshot_denies_as_unavailable() {
        reset_for_test();
        let home = tmp_home("corrupt");
        std::fs::write(snapshot_path(&home), "not json at all").expect("corrupt the snapshot");
        assert!(matches!(
            claim(&home, "lead", 1_000),
            Err(ClaimError::Unavailable { .. })
        ));
        reset_for_test();
        std::fs::remove_dir_all(&home).ok();
    }
}
