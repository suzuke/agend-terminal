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
//! critical section and orders **classify → durable enqueue → record**:
//! 1. a VALID delivered-key for this exact key → [`DeliveryOutcome::Suppressed`]
//!    (persistent dedup, survives restart);
//! 2. else run the durable inbox `enqueue`:
//!    - `Err` → [`DeliveryError::EnqueueFailed`] — no key written, safe to retry;
//!    - `Ok` then record write fails → [`DeliveryError::RecordFailedAfterEnqueue`] —
//!      the message MAY already be delivered and a retry MAY duplicate (ambiguous;
//!      the caller must NOT assume undelivered);
//!    - `Ok` and record durably written ([`crate::store::atomic_write`], fsync file +
//!      parent) → [`DeliveryOutcome::Delivered`].
//!
//! A crash between a successful enqueue and the record write leaves the key absent,
//! so the next attempt re-enqueues (a possible DUPLICATE — NOT bounded across
//! repeated crashes; the consumer is responsible for idempotent consumption).
//! **A missing OR invalid/corrupt/key-mismatched record always means eligible** —
//! never suppress forever merely because a path exists.
//!
//! ## Corruption handling (no lock-free mutation)
//! The public [`is_delivered`] is a PURE read — it never mutates, so it cannot race a
//! concurrent [`deliver_once`] into quarantining a freshly-published record. A
//! corrupt (unparseable) or malformed-timestamp record reads as eligible; it is
//! *quarantined* (moved out of the `.json` namespace) only under the per-key lock,
//! by [`deliver_once`] / [`gc_stale`].
//!
//! ## Exact-head, TTL, no pruning
//! Keys are exact-head-scoped (H and H' are DISTINCT keys). This slice does NOT prune
//! superseded-head keys (removing head H's key would make a stale H caller eligible
//! again) — head invalidation is the obligation layer's job (slice 2). Keys live
//! until [`gc_stale`] reaps them past [`ledger_retention`], which is `>=` the maximum
//! watch/obligation lifetime so a delivered-key always outlives any caller that could
//! re-attempt it.
//!
//! ## Persistence
//! One durable file per key under `home/ci-delivery-ledger/`, written via
//! [`crate::store::atomic_write`] (unique tmp + file `sync_all` + Unix parent-dir
//! `sync_all`), named by a hash of the CANONICAL key only (no raw parts in the path),
//! with a per-key `.lock` sidecar.

// #2741 slice 1 is the delivery FOUNDATION only: this module's API is exercised by
// its own tests and is wired into production by slice 2 (the reviewer-obligation
// reconciler, which calls `deliver_once` and `gc_stale`). Until that lands there is
// deliberately no production caller — allow dead_code so the unwired foundation
// builds clean; the allow is removed when slice 2 wires it.
#![allow(dead_code)]

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

/// Error from [`DeliveryKey::new`] — the key invariants are ENFORCED, not merely
/// claimed.
#[derive(Debug)]
pub(crate) enum KeyError {
    /// `repo` / `reviewer` / `kind` was empty.
    Empty(&'static str),
    /// A field contained a NUL byte (would break the NUL-joined canonical form).
    NulByte(&'static str),
    /// `head_sha` is not a full lowercase-normalizable hex SHA (40 sha1 / 64 sha256).
    BadHeadSha(String),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Empty(field) => write!(f, "delivery key field `{field}` is empty"),
            KeyError::NulByte(field) => write!(f, "delivery key field `{field}` contains NUL"),
            KeyError::BadHeadSha(s) => {
                write!(
                    f,
                    "delivery key head_sha `{s}` is not a full hex SHA (40/64)"
                )
            }
        }
    }
}

impl std::error::Error for KeyError {}

/// The exact-head dedup key: one continuation delivery to one reviewer for one PR
/// head, of one (existing) message kind. Fields are PRIVATE and only constructable
/// via the validating [`DeliveryKey::new`], so the full-hex-SHA / no-NUL invariants
/// the canonical encoding relies on always hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveryKey {
    repo: String,
    pr_number: u64,
    head_sha: String,
    reviewer: String,
    kind: String,
}

impl DeliveryKey {
    /// Validate + normalize. `head_sha` must be a full hex SHA (40 or 64 hex digits;
    /// stored lowercased); `repo`/`reviewer`/`kind` must be non-empty and NUL-free.
    pub(crate) fn new(
        repo: impl Into<String>,
        pr_number: u64,
        head_sha: impl Into<String>,
        reviewer: impl Into<String>,
        kind: impl Into<String>,
    ) -> Result<Self, KeyError> {
        // RED: validation not yet implemented.
        let repo = repo.into();
        let reviewer = reviewer.into();
        let kind = kind.into();
        let head_sha = head_sha.into().to_lowercase();
        Ok(DeliveryKey {
            repo,
            pr_number,
            head_sha,
            reviewer,
            kind,
        })
    }

    /// Canonical, injective serialization: NUL-joined normalized parts. `new`
    /// guarantees no part contains NUL, so this is collision-free.
    fn canonical(&self) -> String {
        format!(
            "{}\0{}\0{}\0{}\0{}",
            self.repo, self.pr_number, self.head_sha, self.reviewer, self.kind
        )
    }
}

/// The persisted delivered-marker. Present + parseable + full-key-equal ⇒ delivered;
/// absent OR corrupt OR key-mismatch ⇒ eligible. `delivered_at` is a TYPED
/// `DateTime<Utc>` so a malformed timestamp fails to deserialize (⇒ corrupt ⇒
/// eligible + quarantine) rather than false-suppressing forever.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DeliveryRecord {
    schema_version: u32,
    repo: String,
    pr_number: u64,
    head_sha: String,
    reviewer: String,
    kind: String,
    delivered_at: String,
}

impl DeliveryRecord {
    fn from_key(key: &DeliveryKey, now: DateTime<Utc>) -> Self {
        DeliveryRecord {
            schema_version: SCHEMA_VERSION,
            repo: key.repo.clone(),
            pr_number: key.pr_number,
            head_sha: key.head_sha.clone(),
            reviewer: key.reviewer.clone(),
            kind: key.kind.clone(),
            delivered_at: now.to_rfc3339(),
        }
    }

    /// Full-key equality against `key` + schema check.
    fn matches(&self, key: &DeliveryKey) -> bool {
        self.schema_version == SCHEMA_VERSION
            && self.repo == key.repo
            && self.pr_number == key.pr_number
            && self.head_sha == key.head_sha
            && self.reviewer == key.reviewer
            && self.kind == key.kind
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

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeliveryError::EnqueueFailed(e) => {
                write!(f, "enqueue failed (not sent; safe to retry): {e}")
            }
            DeliveryError::RecordFailedAfterEnqueue(e) => write!(
                f,
                "delivery record write failed after enqueue (message may be delivered; \
                 retry may duplicate): {e}"
            ),
        }
    }
}

impl std::error::Error for DeliveryError {}

/// Classification of the on-disk record for a key (pure; no mutation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Classification {
    /// A valid record whose full key matches → already delivered.
    Delivered,
    /// No record, unreadable, or a valid record for a DIFFERENT key → eligible.
    Eligible,
    /// A record exists but is unparseable / malformed (e.g. bad timestamp) → eligible,
    /// and should be quarantined by a lock-holding caller.
    Corrupt,
}

fn dir(home: &Path) -> PathBuf {
    home.join("ci-delivery-ledger")
}

/// 32-hex (128-bit) sha256 of the canonical key — filename only; raw parts never
/// appear in the path. A hash collision cannot false-suppress: [`classify`]
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

/// PURE read (no mutation, no lock needed — `atomic_write`'s rename makes a read see
/// old-or-new whole file). Classifies the on-disk record for `key`.
fn classify(home: &Path, key: &DeliveryKey) -> Classification {
    let path = file_for(home, key);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Classification::Eligible, // missing / unreadable
    };
    match serde_json::from_slice::<DeliveryRecord>(&bytes) {
        Ok(rec) if rec.matches(key) => Classification::Delivered,
        Ok(_) => Classification::Eligible, // valid record for a different key
        Err(_) => Classification::Corrupt, // unparseable / malformed timestamp
    }
}

/// Move a corrupt record out of the `.json` namespace (best-effort). MUST be called
/// while holding the key's lock — this is the only mutation of the record path and
/// must not race a concurrent [`deliver_once`] publish.
fn quarantine_corrupt_locked(path: &Path) {
    let dest = path.with_extension("corrupt");
    match std::fs::rename(path, &dest) {
        Ok(()) => {
            tracing::warn!(path = %path.display(), "ci_delivery_ledger: quarantined corrupt record")
        }
        Err(e) => tracing::warn!(
            path = %path.display(), error = %e,
            "ci_delivery_ledger: failed to quarantine corrupt record"
        ),
    }
}

/// True iff a VALID delivered-key exists for this exact key. PURE read: missing /
/// unreadable / corrupt / key-mismatched ⇒ false (eligible). Never mutates — a
/// corrupt record is left for a lock-holding [`deliver_once`] / [`gc_stale`] to
/// quarantine, so this read cannot race a concurrent publish.
pub(crate) fn is_delivered(home: &Path, key: &DeliveryKey) -> bool {
    // RED: lock-free mutating quarantine (the race).
    match classify(home, key) {
        Classification::Delivered => true,
        Classification::Corrupt => {
            quarantine_corrupt_locked(&file_for(home, key));
            false
        }
        Classification::Eligible => false,
    }
}

/// Enqueue-before-record delivery under the per-key lock (which the blocking
/// `acquire_file_lock` also uses to create `dir(home)`). Order:
/// classify → durable enqueue → durable record. A pre-enqueue setup/lock failure
/// returns [`DeliveryError::EnqueueFailed`] (nothing sent; safe to retry).
pub(crate) fn deliver_once<F>(
    home: &Path,
    key: &DeliveryKey,
    now: DateTime<Utc>,
    enqueue: F,
) -> Result<DeliveryOutcome, DeliveryError>
where
    F: FnOnce() -> anyhow::Result<()>,
{
    let _lock = crate::store::acquire_file_lock(&lock_for(home, key))
        .map_err(DeliveryError::EnqueueFailed)?;

    match classify(home, key) {
        Classification::Delivered => return Ok(DeliveryOutcome::Suppressed),
        Classification::Corrupt => quarantine_corrupt_locked(&file_for(home, key)),
        Classification::Eligible => {}
    }

    // Durable enqueue FIRST; failure here means the message was not sent.
    enqueue().map_err(DeliveryError::EnqueueFailed)?;

    // Then the record. A failure here is AMBIGUOUS: the message is already enqueued,
    // so a retry may duplicate — classify it distinctly.
    let rec = DeliveryRecord::from_key(key, now);
    // RED: local unsynced write (no fsync), silent empty on serialize error.
    let bytes = serde_json::to_vec(&rec).unwrap_or_default();
    let path = file_for(home, key);
    let tmp = path.with_extension("json.tmp");
    (|| -> std::io::Result<()> {
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)
    })()
    .map_err(|e| DeliveryError::RecordFailedAfterEnqueue(e.into()))?;
    Ok(DeliveryOutcome::Delivered)
}

/// TTL reap: delete delivered-keys whose `delivered_at` is older than
/// [`ledger_retention`]. Takes the same per-key lock as [`deliver_once`] before
/// mutating; a lock-acquire FAILURE skips that key (never deletes fail-open). A
/// corrupt record is quarantined (under the lock). Best-effort; never panics.
pub(crate) fn gc_stale(home: &Path, now: DateTime<Utc>) {
    let d = dir(home);
    let entries = match std::fs::read_dir(&d) {
        Ok(e) => e,
        Err(_) => return,
    };
    let cutoff = now - ledger_retention();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue; // skip .lock / .tmp / .corrupt
        }
        // Same per-key lock as deliver_once (the .lock shares the .json's hash stem).
        let lock_path = path.with_extension("lock");
        let _lock = crate::store::acquire_file_lock(&lock_path).ok(); // RED: fail-open
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        match serde_json::from_slice::<DeliveryRecord>(&bytes) {
            Ok(rec) => {
                if chrono::DateTime::parse_from_rfc3339(&rec.delivered_at)
                    .map(|d| d.with_timezone(&Utc) < cutoff)
                    .unwrap_or(false)
                {
                    if let Err(e) = std::fs::remove_file(&path) {
                        tracing::warn!(path = %path.display(), error = %e, "ci_delivery_ledger: gc remove failed");
                    }
                }
            }
            Err(_) => quarantine_corrupt_locked(&path), // malformed → quarantine under lock
        }
    }
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

    const SHA0: &str = "0000000000000000000000000000000000000000";
    const SHA1: &str = "1111111111111111111111111111111111111111";

    fn key(head: &str, reviewer: &str) -> DeliveryKey {
        DeliveryKey::new(
            "suzuke/agend-terminal",
            2741,
            head,
            reviewer,
            "ci-ready-for-action",
        )
        .unwrap()
    }

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }
    const NOW: &str = "2026-07-12T06:00:00Z";

    // ---- finding 5: key invariants ENFORCED ----

    #[test]
    fn key_rejects_abbreviated_or_non_hex_sha() {
        assert!(DeliveryKey::new("r", 1, "h0", "rev", "k").is_err());
        assert!(DeliveryKey::new("r", 1, "abcdef12", "rev", "k").is_err());
        assert!(DeliveryKey::new("r", 1, "z".repeat(40), "rev", "k").is_err());
        assert!(DeliveryKey::new("r", 1, SHA0, "rev", "k").is_ok());
    }

    #[test]
    fn key_rejects_empty_or_nul_fields() {
        assert!(DeliveryKey::new("", 1, SHA0, "rev", "k").is_err());
        assert!(DeliveryKey::new("r", 1, SHA0, "", "k").is_err());
        assert!(DeliveryKey::new("r\0x", 1, SHA0, "rev", "k").is_err());
    }

    #[test]
    fn head_sha_is_case_normalized() {
        let home = tmp_home("norm");
        let upper = "ABCDEF1234567890ABCDEF1234567890ABCDEF12";
        let lower = key(&upper.to_lowercase(), "codex-125550");
        let up = key(upper, "codex-125550");
        deliver_once(&home, &lower, t(NOW), || Ok(())).unwrap();
        assert!(
            is_delivered(&home, &up),
            "case variant must dedup as same key"
        );
    }

    // ---- core crash matrix ----

    #[test]
    fn missing_key_is_eligible() {
        let home = tmp_home("missing");
        assert!(!is_delivered(&home, &key(SHA0, "codex-125550")));
    }

    #[test]
    fn deliver_once_enqueues_then_persists_key() {
        let home = tmp_home("persist");
        let k = key(SHA0, "codex-125550");
        let calls = Cell::new(0u32);
        let out = deliver_once(&home, &k, t(NOW), || {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert_eq!(out.unwrap(), DeliveryOutcome::Delivered);
        assert_eq!(calls.get(), 1);
        assert!(is_delivered(&home, &k));
    }

    #[test]
    fn enqueue_failure_writes_no_key_and_retries() {
        let home = tmp_home("efail");
        let k = key(SHA0, "codex-125550");
        let out = deliver_once(&home, &k, t(NOW), || Err(anyhow::anyhow!("io")));
        assert!(matches!(out, Err(DeliveryError::EnqueueFailed(_))));
        assert!(!is_delivered(&home, &k));
        let out2 = deliver_once(&home, &k, t(NOW), || Ok(()));
        assert_eq!(out2.unwrap(), DeliveryOutcome::Delivered);
    }

    /// Record-write failure AFTER a successful enqueue returns the distinct
    /// ambiguous-delivery error. Forced by pre-creating the destination record path
    /// as a DIRECTORY so the atomic write's rename cannot land, while dir(home)
    /// exists so the lock + enqueue still run.
    #[test]
    fn record_failure_after_enqueue_is_distinct_error() {
        let home = tmp_home("rfail");
        let k = key(SHA0, "codex-125550");
        std::fs::create_dir_all(file_for(&home, &k)).unwrap();
        let calls = Cell::new(0u32);
        let out = deliver_once(&home, &k, t(NOW), || {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert_eq!(calls.get(), 1, "enqueue must run before the record write");
        assert!(matches!(
            out,
            Err(DeliveryError::RecordFailedAfterEnqueue(_))
        ));
    }

    #[test]
    fn delivered_key_suppresses_and_persists_across_restart() {
        let home = tmp_home("dedup");
        let k = key(SHA0, "codex-125550");
        deliver_once(&home, &k, t(NOW), || Ok(())).unwrap();
        assert!(is_delivered(&home, &k));
        let out2 = deliver_once(&home, &k, t(NOW), || panic!("must not re-enqueue"));
        assert_eq!(out2.unwrap(), DeliveryOutcome::Suppressed);
    }

    // ---- finding 2: typed timestamp ----

    /// A record with a MATCHING key but a garbage `delivered_at` must NOT
    /// false-suppress: the typed deserialize fails → corrupt → eligible.
    #[test]
    fn garbage_timestamp_record_is_not_delivered() {
        let home = tmp_home("badts");
        let k = key(SHA0, "codex-125550");
        std::fs::create_dir_all(dir(&home)).unwrap();
        let raw = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "repo": "suzuke/agend-terminal", "pr_number": 2741,
            "head_sha": SHA0, "reviewer": "codex-125550", "kind": "ci-ready-for-action",
            "delivered_at": "not-a-timestamp",
        });
        std::fs::write(file_for(&home, &k), serde_json::to_vec(&raw).unwrap()).unwrap();
        assert!(
            !is_delivered(&home, &k),
            "garbage timestamp must not suppress"
        );
    }

    // ---- finding 3: is_delivered is a pure read (no lock-free mutation) ----

    /// The public read must NOT quarantine/mutate — otherwise it can race a
    /// concurrent publish and clobber a valid record. It leaves the corrupt file
    /// in place (quarantine is deferred to a lock-holder).
    #[test]
    fn is_delivered_is_pure_read_does_not_mutate() {
        let home = tmp_home("pureread");
        let k = key(SHA0, "codex-125550");
        std::fs::create_dir_all(dir(&home)).unwrap();
        std::fs::write(file_for(&home, &k), b"{not valid json").unwrap();
        assert!(!is_delivered(&home, &k));
        assert!(
            file_for(&home, &k).exists(),
            "pure read must not quarantine/mutate"
        );
        // idempotent: repeated reads still don't mutate
        assert!(!is_delivered(&home, &k));
        assert!(file_for(&home, &k).exists());
    }

    /// Corrupt records are quarantined under the per-key lock by deliver_once, and
    /// delivery proceeds (corrupt ⇒ eligible).
    #[test]
    fn deliver_once_quarantines_corrupt_under_lock() {
        let home = tmp_home("quar");
        let k = key(SHA0, "codex-125550");
        std::fs::create_dir_all(dir(&home)).unwrap();
        std::fs::write(file_for(&home, &k), b"{corrupt").unwrap();
        let out = deliver_once(&home, &k, t(NOW), || Ok(()));
        assert_eq!(out.unwrap(), DeliveryOutcome::Delivered);
        assert!(
            is_delivered(&home, &k),
            "valid record published over corrupt"
        );
    }

    #[test]
    fn key_mismatch_record_never_false_suppresses() {
        let home = tmp_home("mismatch");
        let k = key(SHA0, "codex-125550");
        std::fs::create_dir_all(dir(&home)).unwrap();
        let wrong = DeliveryRecord {
            schema_version: SCHEMA_VERSION,
            repo: "other/repo".to_string(),
            pr_number: 999,
            head_sha: SHA1.to_string(),
            reviewer: "someone-else".to_string(),
            kind: "ci-ready-for-action".to_string(),
            delivered_at: NOW.to_string(),
        };
        std::fs::write(file_for(&home, &k), serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert!(!is_delivered(&home, &k));
    }

    #[test]
    fn crash_after_enqueue_before_key_redelivers_not_lost() {
        let home = tmp_home("crash-mid");
        let k = key(SHA0, "codex-125550");
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

    #[test]
    fn old_head_key_survives_head_advance() {
        let home = tmp_home("head-move");
        let old = key(SHA0, "codex-125550");
        deliver_once(&home, &old, t(NOW), || Ok(())).unwrap();
        deliver_once(&home, &key(SHA1, "codex-125550"), t(NOW), || Ok(())).unwrap();
        assert!(
            is_delivered(&home, &old),
            "old-head key must survive until TTL"
        );
    }

    // ---- finding 4: GC fails CLOSED on lock failure ----

    #[test]
    fn gc_reaps_only_past_retention() {
        let home = tmp_home("ttl");
        let stale = key(SHA0, "codex-125550");
        let fresh = key(SHA1, "archfix-opus-4");
        deliver_once(&home, &stale, t("2026-07-04T05:00:00Z"), || Ok(())).unwrap();
        deliver_once(&home, &fresh, t("2026-07-12T05:00:00Z"), || Ok(())).unwrap();
        gc_stale(&home, t(NOW));
        assert!(
            !is_delivered(&home, &stale),
            "key past 7d retention must be reaped"
        );
        assert!(is_delivered(&home, &fresh), "fresh key must survive");
    }

    /// GC must NOT delete a record whose per-key lock it cannot acquire (fail-closed).
    /// Forced by pre-creating the `.lock` path as a DIRECTORY so the flock open fails.
    #[test]
    fn gc_skips_when_lock_unavailable() {
        let home = tmp_home("gclock");
        let stale = key(SHA0, "codex-125550");
        deliver_once(&home, &stale, t("2026-07-04T05:00:00Z"), || Ok(())).unwrap();
        // make the lock path un-openable as a file
        let _ = std::fs::remove_file(lock_for(&home, &stale));
        std::fs::create_dir_all(lock_for(&home, &stale)).unwrap();
        gc_stale(&home, t(NOW));
        assert!(
            is_delivered(&home, &stale),
            "stale key must survive when its lock is unavailable (fail-closed)"
        );
    }

    #[test]
    fn concurrent_callers_enqueue_once() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let home = tmp_home("concurrent");
        let k = key(SHA0, "codex-125550");
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
