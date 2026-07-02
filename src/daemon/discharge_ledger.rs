//! #2524 P6 / #2537 PR-1: discharge ledger — persisted record of which
//! `(head_sha, job_name)` notification obligations have been explicitly
//! triaged by an agent, so a future consumer (PR-2) can suppress
//! re-notifying an obligation someone already dealt with.
//!
//! §11.1 / spike `d-20260702101809035049-1`: the ledger MUST be disk-durable.
//! A memory-only cache would replay every triaged obligation as unactioned on
//! the next daemon restart, producing a notification storm — the premise this
//! module's `daemon_refresh_survival` test makes empirical rather than
//! asserted in prose.
//!
//! PR-1 scope (zero behavior change, per the dispatching task): this module
//! is write/read only. Nothing in the notification-decision path
//! (`daemon/poll_reminder.rs`'s `collect_poll_reminders`,
//! `inbox/storage.rs`'s `reclaim_renudge_worthy`) calls `lookup_discharge`
//! yet — that consumer wiring is PR-2's job. GC/TTL for stale head_sha files
//! is likewise out of scope here (no test list entry for it) — noted as a
//! natural PR-3 follow-up, not built now.
//!
//! Signature = `(head_sha, job_name)` only, no repo/branch — per the spike's
//! accepted recommendation, a git SHA is already a globally-unique key, and
//! "head advanced → no ledger entry for the new SHA" gives head-invalidation
//! for free with no separate staleness/TTL check needed at read time.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Canonical path to the discharge-ledger directory — sibling of
/// `ci-watches` (`daemon/ci_watch/registry.rs`'s `ci_watches_dir`), same
/// on-disk convention.
pub(crate) fn discharge_ledger_dir(home: &Path) -> PathBuf {
    home.join("discharge-ledger")
}

/// One head_sha's ledger file. `head_sha` is untrusted MCP input (the
/// `send.triaged.head` field) — not guaranteed to actually be a git SHA — so
/// the filename is `sha256(head_sha)`, mirroring
/// `ci_watch::registry::watch_filename`'s (#943) path-traversal-safe
/// convention rather than using the raw string directly.
fn ledger_path(home: &Path, head_sha: &str) -> PathBuf {
    discharge_ledger_dir(home).join(format!(
        "{}.json",
        crate::daemon::utils::sha256_hex(head_sha.as_bytes())
    ))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DischargeEntry {
    pub discharged_by: String,
    pub discharged_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerFile {
    #[serde(default)]
    jobs: std::collections::BTreeMap<String, DischargeEntry>,
}

/// Record that `job` at `head_sha` was triaged by `discharged_by`. flock +
/// atomic_write read-modify-write, mirroring `ci_watch::registry`'s RMW
/// idiom (e.g. `cleanup_watches_for_instance`).
pub(crate) fn record_discharge(
    home: &Path,
    head_sha: &str,
    job: &str,
    discharged_by: &str,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    let path = ledger_path(home, head_sha);
    let lock_path = path.with_extension("lock");
    let _lock = crate::store::acquire_file_lock(&lock_path)?;
    let mut ledger: LedgerFile = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();
    ledger.jobs.insert(
        job.to_string(),
        DischargeEntry {
            discharged_by: discharged_by.to_string(),
            discharged_at: chrono::Utc::now().to_rfc3339(),
            reason: reason.filter(|s| !s.is_empty()).map(String::from),
        },
    );
    crate::store::save_atomic(&path, &ledger)
}

/// Look up whether `job` at `head_sha` has been discharged. `None` covers
/// both "no ledger file for this head" (head advanced past it — free
/// invalidation, per the spike) and "file exists but this job isn't in it
/// yet". Consumed by `inbox::storage::is_discharged_ci_fail` (#2524 P6-r2).
pub(crate) fn lookup_discharge(home: &Path, head_sha: &str, job: &str) -> Option<DischargeEntry> {
    let path = ledger_path(home, head_sha);
    let ledger: LedgerFile = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())?;
    ledger.jobs.get(job).cloned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(label: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agend-discharge-ledger-{}-{label}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn write_then_read_round_trip() {
        let home = tmp_home("roundtrip");
        record_discharge(&home, "abc123", "ci-build", "dev-1", Some("flaky, reran")).unwrap();

        let entry = lookup_discharge(&home, "abc123", "ci-build").expect("entry must exist");
        assert_eq!(entry.discharged_by, "dev-1");
        assert_eq!(entry.reason.as_deref(), Some("flaky, reran"));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn head_invalidation_different_sha_has_no_entry() {
        let home = tmp_home("head-invalidation");
        record_discharge(&home, "abc123", "ci-build", "dev-1", None).unwrap();

        assert!(
            lookup_discharge(&home, "def456", "ci-build").is_none(),
            "a different head_sha must not see the old head's discharge — head advanced ⟹ free invalidation"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unknown_job_at_known_head_has_no_entry() {
        let home = tmp_home("unknown-job");
        record_discharge(&home, "abc123", "ci-build", "dev-1", None).unwrap();

        assert!(
            lookup_discharge(&home, "abc123", "ci-lint").is_none(),
            "an untriaged job at a known head must read back as None, not the sibling job's entry"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// §11.1: proves the ledger survives a simulated daemon restart — write via
    /// one call, then read via a completely independent call chain with no
    /// shared in-memory state (this module holds none; both calls only share
    /// `home`, i.e. the disk). This is the empirical proof behind the spike's
    /// "must persist to disk, not memory" premise.
    #[test]
    fn daemon_refresh_survival() {
        let home = tmp_home("refresh-survival");
        record_discharge(&home, "abc123", "ci-build", "dev-1", None).unwrap();

        // Simulate a fresh daemon process: a brand new PathBuf pointing at the
        // same on-disk home, no reference to anything from the write above.
        let reopened_home = PathBuf::from(home.to_string_lossy().to_string());
        let entry = lookup_discharge(&reopened_home, "abc123", "ci-build");
        assert!(
            entry.is_some(),
            "entry must be readable from disk alone, independent of the writer's in-memory state"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn record_discharge_overwrites_same_job_idempotently() {
        let home = tmp_home("overwrite");
        record_discharge(&home, "abc123", "ci-build", "dev-1", Some("first")).unwrap();
        record_discharge(&home, "abc123", "ci-build", "dev-2", Some("second")).unwrap();

        let entry = lookup_discharge(&home, "abc123", "ci-build").unwrap();
        assert_eq!(entry.discharged_by, "dev-2", "latest triage wins");
        assert_eq!(entry.reason.as_deref(), Some("second"));

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn record_discharge_preserves_sibling_jobs_at_same_head() {
        let home = tmp_home("sibling-jobs");
        record_discharge(&home, "abc123", "ci-build", "dev-1", None).unwrap();
        record_discharge(&home, "abc123", "ci-lint", "dev-2", None).unwrap();

        assert!(lookup_discharge(&home, "abc123", "ci-build").is_some());
        assert!(lookup_discharge(&home, "abc123", "ci-lint").is_some());

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn empty_reason_is_stored_as_none() {
        let home = tmp_home("empty-reason");
        record_discharge(&home, "abc123", "ci-build", "dev-1", Some("")).unwrap();

        let entry = lookup_discharge(&home, "abc123", "ci-build").unwrap();
        assert_eq!(entry.reason, None, "empty-string reason normalizes to None");

        std::fs::remove_dir_all(&home).ok();
    }
}
