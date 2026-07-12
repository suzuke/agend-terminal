//! #2741 slice 1 — durable at-least-once CI-continuation delivery ledger.
//!
//! A persistent dedup/outbox ledger for the reviewer CI-continuation nudge, keyed
//! by the EXACT head SHA (`repo + pr_number + head_sha + reviewer + kind`). Its sole
//! job is delivery bookkeeping — it holds NO routing policy and introduces NO new
//! wire kind (the `kind` field is an existing inbox message kind, e.g.
//! `"ci-ready-for-action"`). Routing/obligation logic (including head invalidation)
//! lands in a later slice; this ledger is a pure SECOND dedup defense.
//!
//! ## Delivery semantics (per decision d-20260712054213978904-1, r2-revised)
//! Durable **at-least-once + persistent dedup** — NOT exactly-once (the inbox JSONL
//! append and this ledger's key write are separate `fsync`s and cannot be made
//! atomic across a crash). [`deliver_once`] holds the per-key lock across the whole
//! critical section and orders **check → durable enqueue → record**:
//! 1. if a valid delivered-key already exists → [`DeliveryOutcome::Suppressed`]
//!    (persistent dedup, survives restart);
//! 2. else run the durable inbox `enqueue`:
//!    - `Err` → [`DeliveryError::EnqueueFailed`] — no key written, safe to retry;
//!    - `Ok` then record write fails → [`DeliveryError::RecordFailedAfterEnqueue`] —
//!      the message MAY already be delivered and a retry MAY duplicate (ambiguous;
//!      the caller must NOT assume undelivered);
//!    - `Ok` and record persisted → [`DeliveryOutcome::Delivered`].
//!
//! A crash between a successful enqueue and the record write leaves the key absent,
//! so the next attempt re-enqueues (a possible DUPLICATE — NOT bounded across
//! repeated crashes; the consumer is responsible for idempotent consumption).
//! **A missing OR invalid/corrupt key always means eligible** — never suppress
//! forever merely because a path exists.
//!
//! ## Exact-head, TTL, no pruning
//! Keys are exact-head-scoped (H and H' are DISTINCT keys), so a rebase / force-push
//! cannot alias one head's delivery onto another. This slice does NOT prune
//! superseded-head keys (removing head H's key would make a stale H caller eligible
//! again) — head invalidation is the obligation layer's job (slice 2). Keys live
//! until [`gc_stale`] reaps them past [`LEDGER_RETENTION`], which is `>=` the maximum
//! watch/obligation lifetime so a delivered-key always outlives any caller that
//! could re-attempt it.
//!
//! ## Persistence
//! Mirrors [`crate::daemon::ci_handoff_track`]: one atomic (`.tmp`→`rename`) JSON file
//! per key under `home/ci-delivery-ledger/`, named by a hash of the CANONICAL key
//! only (no raw repo/reviewer/kind in the path), a per-key `.lock` sidecar, and a
//! lock-free `.json`-filtered `list()` for the GC sweep.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

/// Delivered-key retention. Reuses the watch absolute age-cap
/// ([`crate::daemon::ci_watch::MAX_WATCH_AGE_HOURS`] = 7d) — the longest a watch, and
/// thus any review obligation that could re-attempt a delivery, can live. A ledger
/// key MUST outlive every caller that might retry for its exact head, so retention
/// is `>=` that bound (not the 24h handoff-track TTL).
pub(crate) fn ledger_retention() -> chrono::Duration {
    chrono::Duration::hours(crate::daemon::ci_watch::MAX_WATCH_AGE_HOURS)
}

const SCHEMA_VERSION: u32 = 1;

/// The exact-head dedup key: one continuation delivery to one reviewer for one PR
/// head, of one (existing) message kind. `head_sha` is compared/stored normalized
/// (lowercased full SHA).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveryKey {
    pub repo: String,
    pub pr_number: u64,
    pub head_sha: String,
    pub reviewer: String,
    /// An EXISTING inbox message kind (e.g. `"ci-ready-for-action"`). No new wire kind.
    pub kind: String,
}

impl DeliveryKey {
    /// Normalized form used for hashing + equality: lowercased full head SHA.
    fn normalized(&self) -> DeliveryKey {
        DeliveryKey {
            repo: self.repo.clone(),
            pr_number: self.pr_number,
            head_sha: self.head_sha.to_lowercase(),
            reviewer: self.reviewer.clone(),
            kind: self.kind.clone(),
        }
    }

    /// Canonical, injective serialization (NUL-joined; NUL cannot appear in any
    /// part) hashed for the filename. Raw parts are NOT placed in the path.
    fn canonical(&self) -> String {
        let n = self.normalized();
        format!(
            "{}\0{}\0{}\0{}\0{}",
            n.repo, n.pr_number, n.head_sha, n.reviewer, n.kind
        )
    }
}

/// The persisted delivered-marker. Present + parseable + full-key-equal ⇒ delivered;
/// absent OR corrupt OR key-mismatch ⇒ eligible.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DeliveryRecord {
    schema_version: u32,
    repo: String,
    pr_number: u64,
    head_sha: String,
    reviewer: String,
    kind: String,
    /// RFC3339 — the TTL age anchor.
    delivered_at: String,
}

impl DeliveryRecord {
    /// Full-key equality against a (normalized) key + schema check.
    fn matches(&self, key: &DeliveryKey) -> bool {
        let n = key.normalized();
        self.schema_version == SCHEMA_VERSION
            && self.repo == n.repo
            && self.pr_number == n.pr_number
            && self.head_sha == n.head_sha
            && self.reviewer == n.reviewer
            && self.kind == n.kind
    }
}

/// A successful [`deliver_once`] classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    /// Enqueue succeeded and the delivered-key was durably persisted.
    Delivered,
    /// A valid delivered-key already existed → dedup suppressed the enqueue.
    Suppressed,
}

/// A [`deliver_once`] failure. The two variants differ in retry safety.
#[derive(Debug)]
pub(crate) enum DeliveryError {
    /// Enqueue returned `Err`; NO key written; the message was not sent — safe to retry.
    EnqueueFailed(anyhow::Error),
    /// Enqueue SUCCEEDED but the delivered-key write failed. The message MAY already
    /// be delivered; a retry MAY duplicate. Ambiguous — do NOT assume undelivered.
    RecordFailedAfterEnqueue(anyhow::Error),
}

fn dir(home: &Path) -> PathBuf {
    home.join("ci-delivery-ledger")
}

/// 32-hex (128-bit) sha256 of the canonical key — filename only; raw parts never
/// appear in the path. A hash collision cannot false-suppress: [`is_delivered`]
/// re-checks full-key equality against the stored record.
fn key_hash(key: &DeliveryKey) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key.canonical().as_bytes());
    hex::encode(&h.finalize()[..16])
}

fn file_for(home: &Path, key: &DeliveryKey) -> PathBuf {
    dir(home).join(format!("{}.json", key_hash(key)))
}

fn lock_for(home: &Path, key: &DeliveryKey) -> PathBuf {
    dir(home).join(format!("{}.lock", key_hash(key)))
}

// ===================================================================
// RED SCAFFOLDING (slice 1): stub bodies so the revised crash-matrix tests
// compile and FAIL. Real bodies land in the GREEN commit. Helpers above are
// real (pure/needed by tests); the three public fns are stubbed.
// ===================================================================

/// True iff a VALID delivered-key exists for this exact key. Missing / unreadable /
/// corrupt / key-mismatched ⇒ false (eligible); corrupt content is quarantined.
pub(crate) fn is_delivered(_home: &Path, _key: &DeliveryKey) -> bool {
    false // RED stub
}

/// Enqueue-before-record delivery primitive under a per-key lock.
pub(crate) fn deliver_once<F>(
    _home: &Path,
    _key: &DeliveryKey,
    _now: DateTime<Utc>,
    enqueue: F,
) -> Result<DeliveryOutcome, DeliveryError>
where
    F: FnOnce() -> anyhow::Result<()>,
{
    // RED stub: runs enqueue, never locks/dedups/persists, never models record failure.
    match enqueue() {
        Ok(()) => Ok(DeliveryOutcome::Delivered),
        Err(e) => Err(DeliveryError::EnqueueFailed(e)),
    }
}

/// TTL reap: delete delivered-keys whose `delivered_at` is older than
/// [`ledger_retention`]. Takes the per-key lock before deleting (no TOCTOU vs a
/// concurrent [`deliver_once`]).
pub(crate) fn gc_stale(_home: &Path, _now: DateTime<Utc>) {
    // RED stub: no-op.
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn tmp_home(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("ci-ledger-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn key(head: &str, reviewer: &str) -> DeliveryKey {
        DeliveryKey {
            repo: "suzuke/agend-terminal".to_string(),
            pr_number: 2741,
            head_sha: head.to_string(),
            reviewer: reviewer.to_string(),
            kind: "ci-ready-for-action".to_string(),
        }
    }

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }
    const NOW: &str = "2026-07-12T06:00:00Z";

    /// A missing key is eligible (never delivered / legacy).
    #[test]
    fn missing_key_is_eligible() {
        let home = tmp_home("missing");
        assert!(!is_delivered(&home, &key("h0", "codex-125550")));
    }

    /// Success: enqueue runs once, the delivered-key persists, a re-check sees it.
    #[test]
    fn deliver_once_enqueues_then_persists_key() {
        let home = tmp_home("persist");
        let k = key("h0", "codex-125550");
        let calls = Cell::new(0u32);
        let out = deliver_once(&home, &k, t(NOW), || {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert_eq!(out.unwrap(), DeliveryOutcome::Delivered);
        assert_eq!(calls.get(), 1);
        assert!(is_delivered(&home, &k));
    }

    /// SHA is compared normalized (case-insensitive full SHA): an upper/lower head
    /// variant is the SAME key.
    #[test]
    fn head_sha_is_case_normalized() {
        let home = tmp_home("norm");
        let lower = key("abcdef12", "codex-125550");
        let upper = key("ABCDEF12", "codex-125550");
        deliver_once(&home, &lower, t(NOW), || Ok(())).unwrap();
        assert!(
            is_delivered(&home, &upper),
            "case variant must dedup as same key"
        );
    }

    /// enqueue failure writes NO key (eligible); a retry then delivers. Proves
    /// enqueue precedes the key write.
    #[test]
    fn enqueue_failure_writes_no_key_and_retries() {
        let home = tmp_home("efail");
        let k = key("h0", "codex-125550");
        let out = deliver_once(&home, &k, t(NOW), || Err(anyhow::anyhow!("io")));
        assert!(matches!(out, Err(DeliveryError::EnqueueFailed(_))));
        assert!(!is_delivered(&home, &k));
        let out2 = deliver_once(&home, &k, t(NOW), || Ok(()));
        assert_eq!(out2.unwrap(), DeliveryOutcome::Delivered);
    }

    /// Record-write failure AFTER a successful enqueue returns the distinct
    /// ambiguous-delivery error (message may already be out; no key written).
    /// Forced deterministically by making the ledger dir a FILE so the record
    /// write cannot create its directory.
    #[test]
    fn record_failure_after_enqueue_is_distinct_error() {
        let home = tmp_home("rfail");
        // ledger dir path is a FILE → any record write fails, enqueue still runs.
        std::fs::write(dir(&home), b"not a dir").unwrap();
        let k = key("h0", "codex-125550");
        let calls = Cell::new(0u32);
        let out = deliver_once(&home, &k, t(NOW), || {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert_eq!(calls.get(), 1, "enqueue must have run");
        assert!(matches!(
            out,
            Err(DeliveryError::RecordFailedAfterEnqueue(_))
        ));
    }

    /// After-key: a valid delivered-key suppresses a second attempt WITHOUT
    /// re-enqueuing, and the dedup survives a restart (fresh disk read).
    #[test]
    fn delivered_key_suppresses_and_persists_across_restart() {
        let home = tmp_home("dedup");
        let k = key("h0", "codex-125550");
        deliver_once(&home, &k, t(NOW), || Ok(())).unwrap();
        assert!(is_delivered(&home, &k));
        let out2 = deliver_once(&home, &k, t(NOW), || panic!("must not re-enqueue"));
        assert_eq!(out2.unwrap(), DeliveryOutcome::Suppressed);
    }

    /// Corrupt / mismatched record must NEVER false-suppress: it reads as eligible
    /// (and is quarantined so it isn't re-parsed every poll).
    #[test]
    fn corrupt_record_never_false_suppresses() {
        let home = tmp_home("corrupt");
        let k = key("h0", "codex-125550");
        std::fs::create_dir_all(dir(&home)).unwrap();
        std::fs::write(file_for(&home, &k), b"{not valid json").unwrap();
        assert!(!is_delivered(&home, &k), "corrupt record must not suppress");
        // quarantined → original path no longer a live .json record
        assert!(
            !file_for(&home, &k).exists(),
            "corrupt record should be quarantined"
        );
    }

    /// A record whose stored key does not equal the lookup key (hash collision /
    /// tampering) must not suppress.
    #[test]
    fn key_mismatch_record_never_false_suppresses() {
        let home = tmp_home("mismatch");
        let k = key("h0", "codex-125550");
        std::fs::create_dir_all(dir(&home)).unwrap();
        let wrong = DeliveryRecord {
            schema_version: SCHEMA_VERSION,
            repo: "other/repo".to_string(),
            pr_number: 999,
            head_sha: "zz".to_string(),
            reviewer: "someone-else".to_string(),
            kind: "ci-ready-for-action".to_string(),
            delivered_at: NOW.to_string(),
        };
        std::fs::write(file_for(&home, &k), serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert!(
            !is_delivered(&home, &k),
            "key-mismatched record must not suppress"
        );
    }

    /// Crash after enqueue before key: the record is lost to a crash; the retry
    /// re-enqueues (a duplicate — NOT lost) and then persists.
    #[test]
    fn crash_after_enqueue_before_key_redelivers_not_lost() {
        let home = tmp_home("crash-mid");
        let k = key("h0", "codex-125550");
        let calls = Cell::new(0u32);
        deliver_once(&home, &k, t(NOW), || {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .unwrap();
        let _ = std::fs::remove_file(file_for(&home, &k)); // simulate lost record
        assert!(!is_delivered(&home, &k));
        let out2 = deliver_once(&home, &k, t(NOW), || {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert_eq!(out2.unwrap(), DeliveryOutcome::Delivered);
        assert_eq!(calls.get(), 2, "redelivery must occur (at-least-once)");
        assert!(is_delivered(&home, &k));
    }

    /// Old-head delivered key SURVIVES a head advance (no pruning in this slice) —
    /// until TTL. Head invalidation is the obligation layer's job.
    #[test]
    fn old_head_key_survives_head_advance() {
        let home = tmp_home("head-move");
        let old = key("h0", "codex-125550");
        deliver_once(&home, &old, t(NOW), || Ok(())).unwrap();
        // a delivery at the new head does not touch the old-head key
        deliver_once(&home, &key("h1", "codex-125550"), t(NOW), || Ok(())).unwrap();
        assert!(
            is_delivered(&home, &old),
            "old-head key must survive until TTL"
        );
    }

    /// GC reaps only keys older than retention (>= 7d); a fresh key survives.
    #[test]
    fn gc_reaps_only_past_retention() {
        let home = tmp_home("ttl");
        let stale = key("h0", "codex-125550");
        let fresh = key("h1", "archfix-opus-4");
        // stale delivered 8 days ago; fresh 1h ago. retention = 7d.
        deliver_once(&home, &stale, t("2026-07-04T05:00:00Z"), || Ok(())).unwrap();
        deliver_once(&home, &fresh, t("2026-07-12T05:00:00Z"), || Ok(())).unwrap();
        gc_stale(&home, t(NOW));
        assert!(
            !is_delivered(&home, &stale),
            "key past 7d retention must be reaped"
        );
        assert!(is_delivered(&home, &fresh), "fresh key must survive");
    }

    /// Concurrent callers for the same key enqueue at most once (per-key lock).
    #[test]
    fn concurrent_callers_enqueue_once() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let home = tmp_home("concurrent");
        let k = key("h0", "codex-125550");
        let count = Arc::new(AtomicU32::new(0));
        let mut handles = vec![];
        for _ in 0..8 {
            let home = home.clone();
            let k = k.clone();
            let count = Arc::clone(&count);
            handles.push(std::thread::spawn(move || {
                let _ = deliver_once(&home, &k, t(NOW), || {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                });
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "per-key lock must enqueue exactly once"
        );
        assert!(is_delivered(&home, &k));
    }
}
