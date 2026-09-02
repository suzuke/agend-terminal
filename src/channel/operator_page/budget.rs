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
//! **Why that is enough.** Because memory is authoritative, deleting or editing
//! the snapshot can never GRANT a slot. The only thing tampering can do is put the
//! budget into a state the daemon will not vouch for — and that state DENIES.
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
    /// The snapshot vanished after initialisation wrote one. That is tampering,
    /// not a fresh install — a fresh install is handled at initialisation.
    SnapshotMissing,
    /// The snapshot could not be read, or could not be created on a fresh home.
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
            Poison::SnapshotMissing => {
                "the operator-page rate snapshot is missing after initialisation wrote one"
            }
            Poison::SnapshotUnusable => {
                "the operator-page rate snapshot could not be read or created"
            }
        }
    }
}

/// Why a claim was refused.
pub(crate) enum ClaimError {
    /// The rolling-hour budget is spent. Ordinary, expected, and retryable.
    RateLimited { retry_after_secs: i64 },
    /// The budget state itself is untrustworthy. NOT a rate cap — see the module
    /// header for why the two must not share a code.
    Unavailable { reason: &'static str },
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
/// ABSENT snapshot ⇒ a genuinely fresh deployment: start empty AND lay one down
/// immediately, which is what makes a later absence provably a deletion.
/// PRESENT and parses ⇒ seed from it. PRESENT and corrupt ⇒ poison.
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
            if let Err(error) = persist(&path, &state.stamps) {
                tracing::error!(
                    snapshot = %path.display(), %error,
                    "operator-page rate snapshot could not be created on a fresh home — budget poisoned"
                );
                state.poison = Some(Poison::SnapshotUnusable);
            }
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
            reason: poison.reason(),
        });
    }

    // Initialisation guarantees a snapshot exists for this home, so an absent file
    // HERE is a deletion. Memory is authoritative, so the deletion cannot refill
    // the budget; it can only mark the state untrustworthy, which denies.
    let path = snapshot_path(home);
    if !path.exists() {
        tracing::error!(
            snapshot = %path.display(),
            "operator-page rate snapshot disappeared after initialisation — budget poisoned, paging denied"
        );
        state.poison = Some(Poison::SnapshotMissing);
        return Err(ClaimError::Unavailable {
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
            reason: "the operator-page rate snapshot could not be written",
        });
    }
    Ok(remaining)
}

/// Give a claimed slot back — used when the claim bought nothing, i.e. the
/// fan-out reached zero channels, so no page was actually delivered.
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

    /// A fresh home lays down a snapshot at initialisation. Without that, a later
    /// absence could not be told apart from a first run.
    #[test]
    #[serial]
    fn fresh_home_writes_an_empty_snapshot() {
        reset_for_test();
        let home = tmp_home("fresh");
        assert!(claim(&home, "lead", 1_000).is_ok());
        assert!(snapshot_path(&home).exists());
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
