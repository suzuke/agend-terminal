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
//! While this process lives, the only thing tampering can do is put the budget
//! into a state the daemon will not vouch for — and that state DENIES. Across a
//! RESTART the picture is different, and the matrix below states it exactly.
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
//! # The tamper matrix: shape × process lifetime
//!
//! Two tampering shapes have now been proved against this module by review —
//! delete-plus-restart, then a syntactically VALID rewrite plus restart. Fixing
//! a third cell would just invite a fourth, so what follows is the general RULE
//! and the whole class.
//!
//! **The rule.** In a LIVE process every shape is CLOSED, because memory is
//! authoritative and the claim path never consults the file for a count. After a
//! RESTART memory is gone and the file is the only witness, so one question
//! decides everything: do the bytes PARSE as `BTreeMap<String, Vec<i64>>`?
//!
//!   * ABSENT, UNPARSEABLE or UNREADABLE ⇒ the daemon refuses to invent a count
//!     and fails CLOSED ([`ClaimError::Unavailable`], with a `cause`).
//!   * PARSEABLE ⇒ the daemon TRUSTS it, whatever it says. There is no integrity
//!     check and there cannot be a meaningful one here (below), so EVERY
//!     parseable rewrite is an ACCEPTED limitation, not a defect to fix.
//!
//! | shape on disk | while the daemon runs | after a restart |
//! |---|---|---|
//! | deleted | CLOSED — `snapshot_missing` denies | CLOSED — `snapshot_absent` denies |
//! | corrupt / unparseable | CLOSED — never read for a count | CLOSED — `snapshot_corrupt` denies |
//! | truncated | CLOSED — never read for a count | CLOSED — unparseable ⇒ `snapshot_corrupt` |
//! | unreadable (e.g. path replaced by a directory) | CLOSED — never read for a count | CLOSED — `snapshot_unusable` denies |
//! | unwritable (the persist fails) | CLOSED — claim rolled back, `snapshot_unwritable` denies | CLOSED — identical |
//! | rewritten to a valid `{}` | CLOSED — memory is authoritative | **ACCEPTED** — the hour IS refilled |
//! | PARTIAL valid rewrite (one orchestrator's stamps dropped) | CLOSED — memory is authoritative | **ACCEPTED** — that orchestrator's hour IS refilled |
//! | STALE honest snapshot restored | CLOSED — memory is authoritative | **ACCEPTED** — the older count is believed |
//! | future-dated stamps | CLOSED — memory is authoritative | CLOSED — future stamps are KEPT, not pruned, so they still occupy the hour |
//!
//! Every CLOSED cell has a test and every ACCEPTED cell has an honestly named
//! pin, which must be INVERTED rather than deleted if this ever changes. The
//! live column is `every_tampering_shape_is_closed_while_the_process_lives`; the
//! restart column is `restart_with_absent_or_unparseable_state_fails_closed` and
//! `restart_with_any_parseable_rewrite_is_trusted_by_design`, plus
//! `valid_snapshot_rewrite_plus_restart_refills_the_hour_by_design` driving the
//! real MCP entry in `operator_page/tests.rs`.
//!
//! **Why there is no integrity mechanism — and why DETECTION fails too.** Start
//! with what makes the question moot: a seat that can rewrite this file has,
//! per `docs/architecture.md` §1.2, already read `api.operator` and can message
//! the operator directly. Hardening this cap against that seat protects nothing,
//! because a strictly better bypass is already in its hands. Even setting that
//! aside, the mechanism cannot be built: signing the snapshot needs a key the
//! daemon can read at boot with no operator present.
//! Per `docs/architecture.md` §1.2 a hostile same-UID seat can already read
//! same-owner credentials and rewrite same-owner runtime state, so it can read
//! that key and MAC `{}` itself. Detection fails for exactly the reason
//! prevention does: a forged record is indistinguishable from an honest one.
//!
//! **Why not fail CLOSED at every restart instead** — restore no remaining budget
//! and treat the hour as spent after each daemon start? That would make
//! "tampering can only deny" literally true. It is rejected on evidence. This
//! deployment's event log carries 40 `daemon_start` events between 2026-08-24 and
//! 2026-09-02, one every ~5.5 hours; blanking paging for up to an hour after each
//! leaves the operator unpageable roughly 18% of the time, in a feature whose
//! whole purpose is reaching them overnight. It also converts "an attacker gains
//! three extra pages" into "an attacker OR ANY CRASH silences the operator's
//! phone for an hour", and for an ALERTING feature suppression is the worse
//! failure mode. Under §1.2 an agent seat is trusted code running as the
//! operator's own OS user: this cap's job is to bound ACCIDENT and runaway, not
//! malice.
//!
//! **So, plainly.** The persisted 3/hour cap is NOT an independent same-UID
//! damage bound and must not be described as one. It is defence in depth against
//! an accident, a clumsy reset and a crash. What bounds a hostile seat is the set
//! of controls that do not live in an agent-writable file: default-OFF, the
//! operator-gated switch, the dedicated topic, and the fact that every page is
//! attributed and lands in front of the operator.
//!
//! **What is NOT withdrawn.** The durability requirement — the counter must
//! survive a daemon restart, or the restart itself becomes the bypass — is still
//! met: an honest restart preserves the spent hour. Provenance, because it was
//! got wrong once already and the mistake drove four rounds of work: that
//! sentence is the ORCHESTRATOR's expansion in the implementation task, not the
//! operator's words. The operator decision (d-20260902104216571473-11) settles
//! the 3/hour cap, the DROP semantics and the refusal, and says nothing about
//! restarts. What is withdrawn is the STRONGER claim this module added on top,
//! that a hostile agent cannot refill the hour by itself. That one was false,
//! and it is gone.
//!
//! **What the operator's remedy costs, said out loud.** Re-seeding after a
//! snapshot that was ABSENT or CORRUPT at initialisation writes `{}` and hands
//! back a full budget INSIDE the current hour: by that point the spent count has
//! been destroyed with the snapshot and this process has nothing to restore it
//! from. So an agent that deletes the file and gets the daemon restarted CAN, in
//! effect, have the hour reset — but only by persuading the operator to run an
//! operator-only command, and only after every page in between has been denied.
//! A second adversarial pass showed the old remedy hint did not mention this at
//! all, which is the part that was unacceptable: the reset itself is a fair price
//! for a control that denies by default, being asked to perform it blind is not.
//! [`seed_snapshot`] now logs the reset at `warn!` and the refusal hint states it
//! before the operator acts.
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
///
/// This function is the whole PARSEABLE ⇒ TRUSTED half of the module header's
/// matrix, and it carries NO integrity check by design: any well-formed map is
/// believed, `{}` included. A MAC would be verified with a key a same-UID agent
/// can read and forge with, so it would stop nobody while adding the appearance
/// of a control. Deletion, corruption, truncation and unreadability are the only
/// tamper shapes anything here can catch — and all four deny.
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
/// `paging_was_already_enabled` is the caller's answer to "had the operator
/// turned paging on for this home BEFORE this call?". It is the only signal that
/// separates a genuine first-ever seed from a RE-seed after the snapshot went
/// away, because a restart destroys the in-memory poison latch too. Its one blind
/// spot is deliberate and narrow: enable → DISABLE → delete → enable seeds with
/// the flag false and is logged as a first seed. Nothing on disk or in memory
/// distinguishes that from a fresh install, and it costs an operator act at both
/// ends.
///
/// The honest limit is the same one the module header's matrix states: under one
/// shared OS user this file is not tamper-proof, and only the ABSENT, CORRUPT,
/// TRUNCATED and UNREADABLE shapes are even tamper-EVIDENT — a parseable rewrite
/// is not, so this operator remedy is the answer to the evident shapes and to
/// accidents, not a control against a hostile seat. Where the in-memory count is
/// gone too — absent or corrupt at initialisation, or a restart before the first
/// claim — re-seeding genuinely DOES start the rolling hour over. That is accepted (only
/// an operator can do it, and the default is to deny), but it is never silent: it
/// is logged at `warn!` here, and the refusal that sent the operator to this
/// command says so before they run it.
pub(crate) fn seed_snapshot(home: &Path, paging_was_already_enabled: bool) -> anyhow::Result<()> {
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
        // Already seeded: leave it alone. This is the no-clobber guarantee. No
        // counter changed, so there is no new hour to announce either.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let initialised_for_home = state.initialised && state.home == home;
    // Is the spent count for this home GONE? It is whenever this process never
    // built state from a real snapshot for the home: either it has not
    // initialised for it at all (a restart, before the first claim), or it did
    // and the snapshot was ABSENT or CORRUPT at that moment, so memory was never
    // populated. `SnapshotMissing` and a healthy state are the opposite case —
    // memory holds the truth and the write-back below restores it byte-for-byte.
    let spent_count_lost = !initialised_for_home
        || matches!(
            state.poison,
            Some(Poison::SnapshotAbsent) | Some(Poison::CorruptSnapshot)
        );
    // …and `paging_was_already_enabled` is what makes this a RE-seed rather than a
    // genuine first-ever one. A first enable for a home has no hour to lose.
    let starts_new_hour = paging_was_already_enabled && spent_count_lost;
    if initialised_for_home {
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
    if starts_new_hour {
        tracing::warn!(
            snapshot = %path.display(),
            "operator-page rate snapshot re-seeded EMPTY — the spent count for this home was destroyed with the old snapshot, so the rolling hour STARTS NOW; pages spent before this are forgotten"
        );
    } else {
        tracing::info!(
            snapshot = %path.display(),
            "operator-page rate snapshot seeded by the operator"
        );
    }
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
        seed_snapshot(&home, false).expect("operator seeds the snapshot");
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
        seed_snapshot(&home, false).expect("seed is a no-op here");
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
        seed_snapshot(&home, false).expect("operator seeds the snapshot");
        for _ in 0..RATE_LIMIT_PER_HOUR {
            assert!(claim(&home, "lead", 1_000).is_ok());
        }
        std::fs::remove_file(snapshot_path(&home)).expect("delete the snapshot");
        assert!(matches!(
            claim(&home, "lead", 1_000),
            Err(ClaimError::Unavailable { .. })
        ));
        seed_snapshot(&home, true).expect("operator re-seeds");
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

    /// The operator's remedy is ALLOWED to start a new rolling hour — by the time
    /// an absent or corrupt snapshot is noticed, the spent count has been destroyed
    /// with it and nothing in this process can reconstruct it. What it may not do
    /// is start one SILENTLY, which is what a second adversarial pass caught: the
    /// hint sent the operator to `admin config-set operator_page.enabled true`
    /// without saying that re-running it hands back a full budget inside the same
    /// hour. This pins both ends of that: a genuine FIRST seed says nothing about a
    /// reset, and a RE-seed after the count was lost warns.
    ///
    /// Nothing here makes the snapshot tamper-proof, and only the ABSENT, CORRUPT,
    /// TRUNCATED and UNREADABLE shapes are tamper-EVIDENT at all (module header
    /// matrix). This operator-gated reset is the remedy for those and for
    /// accidents; it is not a control against a hostile seat.
    #[test]
    #[serial]
    #[tracing_test::traced_test]
    fn re_seeding_after_the_count_was_lost_warns_about_the_new_rolling_hour() {
        reset_for_test();
        let home = tmp_home("new-hour-log");

        seed_snapshot(&home, false).expect("the operator enables paging for the first time");
        assert!(
            !logs_contain("STARTS NOW"),
            "a first-ever seed has no hour to lose and must not be reported as a reset"
        );
        for _ in 0..RATE_LIMIT_PER_HOUR {
            assert!(claim(&home, "lead", 1_000).is_ok());
        }

        // The proved scenario: an agent deletes the snapshot and the daemon
        // restarts, so the spent count is gone from memory as well as from disk.
        std::fs::remove_file(snapshot_path(&home)).expect("delete the snapshot");
        reset_for_test();
        assert!(
            matches!(
                claim(&home, "lead", 1_000),
                Err(ClaimError::Unavailable { cause, .. }) if cause == "snapshot_absent"
            ),
            "the deletion must DENY first — the reset is only reachable through the operator"
        );

        seed_snapshot(&home, true).expect("the operator re-runs the enable command");

        assert!(
            logs_contain("rolling hour STARTS NOW"),
            "re-seeding after the count was destroyed must warn that the hour restarts"
        );
        assert!(
            claim(&home, "lead", 1_000).is_ok(),
            "…and it really does restart — the warning must describe what happens, not soften it"
        );
        reset_for_test();
        std::fs::remove_dir_all(&home).ok();
    }

    /// LIVE PROCESS ONLY. While this daemon runs, memory is the authority, so
    /// rewriting the snapshot with an empty map does NOT hand back a spent slot.
    ///
    /// That is the whole of what this pins, and it must not be read as more. The
    /// SAME rewrite followed by a RESTART does refill the hour — the accepted
    /// limitation in the module header's matrix, pinned by
    /// [`restart_with_any_parseable_rewrite_is_trusted_by_design`] below and by
    /// `valid_snapshot_rewrite_plus_restart_refills_the_hour_by_design` at the
    /// real MCP entry.
    #[test]
    #[serial]
    fn editing_the_snapshot_cannot_grant_a_slot() {
        reset_for_test();
        let home = tmp_home("edit");
        seed_snapshot(&home, false).expect("operator seeds the snapshot");
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
        seed_snapshot(&home, false).expect("operator seeds the snapshot");
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

    // ── the tamper matrix (module header) ────────────────────────────────
    //
    // Two review rounds each proved ONE tampering shape (delete+restart, then a
    // valid rewrite+restart). These three cases replace cell-by-cell whack-a-mole
    // with the class: every shape below is exercised in BOTH lifetime columns,
    // and each cell is either CLOSED with an assertion or ACCEPTED with a pin.

    /// One on-disk tampering shape. `Unwritable` is not a content shape and is
    /// pinned separately by [`an_unwritable_snapshot_denies_and_rolls_the_claim_back`].
    #[derive(Clone, Copy, Debug)]
    enum Shape {
        Deleted,
        Corrupt,
        Truncated,
        /// The snapshot path replaced by a DIRECTORY. Portable and uid-proof
        /// (a `chmod 000` test is a no-op for root, which CI containers often
        /// are), and it makes both the read and the rename fail.
        Unreadable,
        EmptyRewrite,
        /// One orchestrator's stamps removed, another's kept — still valid JSON.
        PartialRewrite,
        /// An older HONEST snapshot put back, taken before the hour was spent.
        StaleRestore,
        FutureDated,
    }

    const SHAPES: [Shape; 8] = [
        Shape::Deleted,
        Shape::Corrupt,
        Shape::Truncated,
        Shape::Unreadable,
        Shape::EmptyRewrite,
        Shape::PartialRewrite,
        Shape::StaleRestore,
        Shape::FutureDated,
    ];

    fn apply(shape: Shape, path: &Path, now: i64) {
        match shape {
            Shape::Deleted => std::fs::remove_file(path).expect("delete the snapshot"),
            Shape::Corrupt => std::fs::write(path, "not json at all").expect("corrupt it"),
            // A prefix of a real snapshot: still bytes, no longer JSON.
            Shape::Truncated => std::fs::write(path, "{\n  \"lead\": [1").expect("truncate it"),
            Shape::Unreadable => {
                std::fs::remove_file(path).expect("clear the snapshot");
                std::fs::create_dir(path).expect("replace the snapshot with a directory");
            }
            Shape::EmptyRewrite => std::fs::write(path, "{}").expect("blank it"),
            Shape::PartialRewrite => {
                std::fs::write(path, serde_json::json!({ "other": [now] }).to_string())
                    .expect("drop lead's stamps, keep another orchestrator's")
            }
            Shape::StaleRestore => {
                std::fs::write(path, serde_json::json!({ "lead": [now] }).to_string())
                    .expect("restore an older honest snapshot")
            }
            Shape::FutureDated => std::fs::write(
                path,
                serde_json::json!({ "lead": [now + 9_000, now + 9_001, now + 9_002] }).to_string(),
            )
            .expect("write future-dated stamps"),
        }
    }

    /// Seed `home`, spend `lead`'s whole hour, then apply `shape`.
    fn spent_home_with(shape: Shape, tag: &str) -> PathBuf {
        let home = tmp_home(tag);
        seed_snapshot(&home, false).expect("operator seeds the snapshot");
        for _ in 0..RATE_LIMIT_PER_HOUR {
            assert!(claim(&home, "lead", 1_000).is_ok(), "{shape:?}");
        }
        apply(shape, &snapshot_path(&home), 1_000);
        home
    }

    /// MATRIX, LIVE COLUMN — every shape is CLOSED, and this is a general
    /// argument rather than eight coincidences: the claim path never consults the
    /// file for a count while the process is initialised for the home, so what the
    /// bytes say cannot matter. The only thing a shape can change is WHICH refusal
    /// comes back (a deletion is noticed by the existence probe and escalates to
    /// `snapshot_missing`); none of them can turn a refusal into a grant.
    #[test]
    #[serial]
    fn every_tampering_shape_is_closed_while_the_process_lives() {
        for shape in SHAPES {
            reset_for_test();
            let home = spent_home_with(shape, "live-matrix");
            match claim(&home, "lead", 1_000) {
                Err(ClaimError::RateLimited { .. }) => {}
                Err(ClaimError::Unavailable { cause, .. }) => assert_eq!(
                    cause, "snapshot_missing",
                    "only a DELETED snapshot is noticed on the live claim path: {shape:?}"
                ),
                Ok(_) => panic!("{shape:?} must not grant a slot while the process lives"),
            }
            reset_for_test();
            std::fs::remove_dir_all(&home).ok();
        }
    }

    /// MATRIX, RESTART COLUMN — the CLOSED half. After a restart the file is the
    /// only witness left, and a daemon that cannot PARSE one refuses to invent a
    /// count. Absent, corrupt, truncated and unreadable therefore all deny, each
    /// with its own cause so the operator can tell them apart.
    #[test]
    #[serial]
    fn restart_with_absent_or_unparseable_state_fails_closed() {
        for (shape, expected) in [
            (Shape::Deleted, "snapshot_absent"),
            (Shape::Corrupt, "snapshot_corrupt"),
            (Shape::Truncated, "snapshot_corrupt"),
            (Shape::Unreadable, "snapshot_unusable"),
        ] {
            reset_for_test();
            let home = spent_home_with(shape, "restart-closed");
            // The restart: memory is gone, so the file decides.
            reset_for_test();
            match claim(&home, "lead", 1_000) {
                Err(ClaimError::Unavailable { cause, .. }) => assert_eq!(
                    cause, expected,
                    "{shape:?} must deny after a restart with its own cause"
                ),
                _ => panic!("{shape:?} must fail CLOSED after a restart"),
            }
            reset_for_test();
            std::fs::remove_dir_all(&home).ok();
        }
    }

    /// MATRIX, RESTART COLUMN — the ACCEPTED half, named so it can never be
    /// mistaken for a security control that passes.
    ///
    /// Anything that PARSES is TRUSTED after a restart, whatever it says. The only
    /// defence against a rewrite was "memory is authoritative", and memory dies
    /// with the process; the file carries no integrity material because any key
    /// this daemon can read at boot is readable — and forgeable — by a same-UID
    /// agent (`docs/architecture.md` §1.2). So a spent hour really is refilled by
    /// a blank rewrite, by a partial one, and by restoring an older honest file.
    /// This is the DOCUMENTED, ACCEPTED limitation of the persisted cap: it bounds
    /// accident and runaway, not a hostile seat.
    ///
    /// IF AN INTEGRITY MECHANISM IS EVER ADDED, INVERT THIS TEST — do not delete
    /// it. The assertion that a rewrite is trusted is exactly the thing that would
    /// have to start failing.
    #[test]
    #[serial]
    fn restart_with_any_parseable_rewrite_is_trusted_by_design() {
        for shape in [
            Shape::EmptyRewrite,
            Shape::PartialRewrite,
            Shape::StaleRestore,
        ] {
            reset_for_test();
            let home = spent_home_with(shape, "restart-accepted");
            reset_for_test();
            assert!(
                claim(&home, "lead", 1_000).is_ok(),
                "{shape:?} plus a restart DOES refill the hour — accepted, not fixed"
            );
            reset_for_test();
            std::fs::remove_dir_all(&home).ok();
        }

        // The one parseable shape that buys nothing: stamps dated in the FUTURE
        // are kept rather than pruned, so "write the hour forward" stays closed.
        reset_for_test();
        let home = spent_home_with(Shape::FutureDated, "restart-future");
        reset_for_test();
        assert!(
            matches!(
                claim(&home, "lead", 1_000),
                Err(ClaimError::RateLimited { .. })
            ),
            "future-dated stamps must still occupy the hour after a restart"
        );
        reset_for_test();
        std::fs::remove_dir_all(&home).ok();
    }

    /// MATRIX: the UNWRITABLE cell, which behaves identically in both columns. A
    /// counter this process cannot snapshot is a counter the next process would
    /// not see, which is precisely the bypass the snapshot exists to prevent — so
    /// the claim is rolled back and refused rather than let through uncounted.
    #[test]
    #[serial]
    fn an_unwritable_snapshot_denies_and_rolls_the_claim_back() {
        reset_for_test();
        let home = tmp_home("unwritable");
        seed_snapshot(&home, false).expect("operator seeds the snapshot");
        crate::store::fail_next_atomic_write_for_test(&snapshot_path(&home));
        match claim(&home, "lead", 1_000) {
            Err(ClaimError::Unavailable { cause, .. }) => assert_eq!(cause, "snapshot_unwritable"),
            _ => panic!("a snapshot that cannot be written must deny"),
        }
        assert!(
            claim(&home, "lead", 1_000).is_ok(),
            "the refused claim must have been rolled back, not silently spent"
        );
        reset_for_test();
        std::fs::remove_dir_all(&home).ok();
    }
}
