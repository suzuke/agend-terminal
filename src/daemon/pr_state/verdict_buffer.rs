//! Review evidence buffers have two deliberately disjoint namespaces:
//!
//! - `validated-verdict-buffer/` stores server-validated, assignment-bound
//!   receipts that preceded creation of their exact PR state. CI observation
//!   drains them only while holding the assignment lock and revalidates the
//!   active generation before applying them.
//! - `verdict-buffer/` is the pre-task66 name+SHA sidecar. It remains readable
//!   only by compatibility tests and the TTL sweeper; production never replays
//!   it as review authority.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A SHA that never resolves to a branch head is dropped after this. 24 h
/// matches the `ci_handoff_track` TTL (#1888) — the same orphan-signal horizon.
const TTL_HOURS: i64 = 24;

/// task66 typed buffer. It is intentionally a separate namespace from the
/// legacy name+SHA buffer: old entries can expire but can never be replayed as
/// receipt authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BufferedValidatedReceipt {
    receipt: crate::review_receipt::ReviewReceiptSummary,
    buffered_at: String,
}

fn validated_buffer_dir(home: &Path) -> PathBuf {
    super::pr_state_dir(home).join("validated-verdict-buffer")
}

/// Non-destructive hint used to decide whether CI observation must acquire the
/// assignment lock even when the first authority probe says `Absent`. Exact PR
/// number matching still happens during the locked drain; this only avoids
/// creating assignment-lock sidecars for ordinary branches with no typed buffer.
pub(crate) fn has_validated_subject_hint(
    home: &Path,
    repo: &str,
    branch: &str,
    head: &str,
) -> bool {
    std::fs::read_dir(validated_buffer_dir(home))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<BufferedValidatedReceipt>(&bytes).ok())
        .any(|buffered| {
            buffered.receipt.repo == repo
                && buffered.receipt.branch == branch
                && buffered.receipt.reviewed_head == head
        })
}

/// Buffer one server-validated receipt. Returns true only for a newly accepted
/// receipt/source identity; replays and conflicting reuse are inert.
pub(crate) fn buffer_validated(
    home: &Path,
    receipt: &crate::review_receipt::ReviewReceiptSummary,
) -> bool {
    let dir = validated_buffer_dir(home);
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if !is_buffer_file(&path) {
            continue;
        }
        let Some(existing) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<BufferedValidatedReceipt>(&b).ok())
        else {
            continue;
        };
        if existing.receipt.receipt_id == receipt.receipt_id
            || existing.receipt.source_id == receipt.source_id
        {
            return false;
        }
    }
    let entry = BufferedValidatedReceipt {
        receipt: receipt.clone(),
        buffered_at: chrono::Utc::now().to_rfc3339(),
    };
    let name = format!(
        "{}--{}.json",
        sanitize(&receipt.receipt_id),
        receipt.assignment_id
    );
    let final_path = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    let Ok(bytes) = serde_json::to_vec_pretty(&entry) else {
        return false;
    };
    if std::fs::write(&tmp, bytes).is_err() || std::fs::rename(&tmp, &final_path).is_err() {
        let _ = std::fs::remove_file(tmp);
        return false;
    }
    true
}

/// Drain only receipts whose full subject exactly equals the newly observed PR.
/// No SHA-prefix or cross-PR scan is permitted.
pub(crate) fn drain_validated_for_subject(
    home: &Path,
    repo: &str,
    branch: &str,
    pr_number: u64,
    head: &str,
) -> Vec<crate::review_receipt::ReviewReceiptSummary> {
    let dir = validated_buffer_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_buffer_file(&path) {
            continue;
        }
        let Some(buffered) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<BufferedValidatedReceipt>(&b).ok())
        else {
            continue;
        };
        let receipt = buffered.receipt;
        if receipt.repo == repo
            && receipt.branch == branch
            && receipt.pr_number == pr_number
            && receipt.reviewed_head == head
        {
            let _ = std::fs::remove_file(&path);
            out.push(receipt);
        }
    }
    out
}

/// An owned snapshot of a verdict, parked until its pr-state appears. `kind` is
/// the lowercased verdict word (`VerdictKind` carries a borrow, so it can't be
/// stored directly); [`buffered_kind`] reconstructs the borrowed form on replay.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct BufferedVerdict {
    pub reviewed_head: String,
    pub reviewer: String,
    /// `"verified" | "rejected" | "unverified"`.
    pub kind: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub buffered_at: String,
}

impl BufferedVerdict {
    /// Reconstruct the borrowed [`super::VerdictKind`] for replay. Unknown kind
    /// strings map to `Unverified` (the evidence-exempt, never-merge-ready
    /// verdict) — a corrupt buffer entry can never spuriously flip merge-ready.
    #[cfg(test)]
    pub(crate) fn verdict_kind(&self) -> super::VerdictKind<'_> {
        match self.kind.as_str() {
            "verified" => super::VerdictKind::Verified,
            "rejected" => super::VerdictKind::Rejected {
                reason: self.reason.as_deref(),
            },
            _ => super::VerdictKind::Unverified,
        }
    }
}

fn buffer_dir(home: &Path) -> PathBuf {
    super::pr_state_dir(home).join("verdict-buffer")
}

/// Filename-safe token. Hex SHAs and agent names are already safe; this is a
/// defensive guard against an unexpected character reaching the path.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Buffer a verdict keyed by (reviewed_head, reviewer). Idempotent per key (a
/// re-buffer of the same reviewer+SHA overwrites). Best-effort: errors are
/// logged, never propagated — the verdict path must stay non-fragile.
#[cfg(test)]
pub(crate) fn buffer(
    home: &Path,
    reviewed_head: &str,
    reviewer: &str,
    kind: &str,
    reason: Option<&str>,
) {
    let v = BufferedVerdict {
        reviewed_head: reviewed_head.to_string(),
        reviewer: reviewer.to_string(),
        kind: kind.to_string(),
        reason: reason.map(String::from),
        buffered_at: chrono::Utc::now().to_rfc3339(),
    };
    match write_atomic(home, &v) {
        Ok(()) => tracing::info!(
            reviewed_head,
            reviewer,
            kind,
            "task66 legacy verdict_buffer: buffered display-only verdict; production never replays it"
        ),
        Err(e) => tracing::warn!(reviewed_head, reviewer, error = %e, "#2059 verdict_buffer: write failed"),
    }
}

#[cfg(test)]
fn write_atomic(home: &Path, v: &BufferedVerdict) -> std::io::Result<()> {
    let dir = buffer_dir(home);
    std::fs::create_dir_all(&dir)?;
    let name = format!(
        "{}--{}.json",
        sanitize(&v.reviewed_head),
        sanitize(&v.reviewer)
    );
    let final_path = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(&tmp, &final_path)
}

/// Test-only drain for the legacy namespace. Production never calls this; a SHA
/// match is insufficient review authority after task66.
#[cfg(test)]
pub(crate) fn drain_for_head(home: &Path, head: &str) -> Vec<BufferedVerdict> {
    let dir = buffer_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_buffer_file(&path) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<BufferedVerdict>(&content) else {
            continue;
        };
        // #2079: prefix-tolerant — a buffered verdict whose `reviewed_head` is an
        // abbreviated SHA (e.g. `7e1d422`) drains when the full canonical `head`
        // is observed (the #2078 silent-buffer bug: short reviewed_head never met
        // the full-SHA drain key). `head` is canonical-full; `v.reviewed_head` is
        // what the reviewer asserted.
        if super::sha_prefix_match(head, &v.reviewed_head) {
            let _ = std::fs::remove_file(&path);
            out.push(v);
        }
    }
    out
}

/// Drop buffered verdicts older than [`TTL_HOURS`]. A verdict whose SHA never
/// becomes any branch's head (abandoned PR, force-push past it) must not leak.
/// Wired into the hourly retention sweep. Returns the count removed.
pub(crate) fn sweep_expired(home: &Path, now: chrono::DateTime<chrono::Utc>) -> usize {
    let mut removed = 0;
    for (dir, typed) in [
        (buffer_dir(home), false),
        (validated_buffer_dir(home), true),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_buffer_file(&path) {
                continue;
            }
            let timestamp = std::fs::read(&path).ok().and_then(|bytes| {
                if typed {
                    serde_json::from_slice::<BufferedValidatedReceipt>(&bytes)
                        .ok()
                        .map(|v| v.buffered_at)
                } else {
                    serde_json::from_slice::<BufferedVerdict>(&bytes)
                        .ok()
                        .map(|v| v.buffered_at)
                }
            });
            let expired = timestamp
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(&ts).ok())
                .map(|t| {
                    now.signed_duration_since(t.with_timezone(&chrono::Utc))
                        > chrono::Duration::hours(TTL_HOURS)
                })
                .unwrap_or(true);
            if expired {
                let _ = std::fs::remove_file(&path);
                removed += 1;
            }
        }
    }
    removed
}

/// A buffer file is a `*.json` that is not a dotfile (`.tmp` write-staging).
fn is_buffer_file(path: &Path) -> bool {
    let is_dotfile = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'));
    !is_dotfile && path.extension().and_then(|e| e.to_str()) == Some("json")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "agend-vbuf-{}-{}-{}",
            tag,
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// buffer → drain_for_head round-trip: the matching SHA is returned + removed;
    /// a non-matching SHA leaves its entry parked.
    #[test]
    fn buffer_drains_matching_sha_only_2059() {
        let home = tmp_home("drain");
        buffer(&home, "sha-A", "reviewer-1", "verified", None);
        buffer(&home, "sha-B", "reviewer-2", "rejected", Some("needs r1"));

        let drained = drain_for_head(&home, "sha-A");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].reviewer, "reviewer-1");
        assert!(matches!(
            drained[0].verdict_kind(),
            super::super::VerdictKind::Verified
        ));
        // sha-A removed; sha-B still parked.
        assert!(drain_for_head(&home, "sha-A").is_empty());
        let b = drain_for_head(&home, "sha-B");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].reason.as_deref(), Some("needs r1"));

        std::fs::remove_dir_all(&home).ok();
    }

    /// TTL sweep removes a stale entry, keeps a fresh one.
    #[test]
    fn sweep_expired_drops_stale_keeps_fresh_2059() {
        let home = tmp_home("sweep");
        buffer(&home, "sha-fresh", "rv", "verified", None);
        // Hand-write a stale entry (buffered_at 48h ago).
        let stale = BufferedVerdict {
            reviewed_head: "sha-stale".into(),
            reviewer: "rv".into(),
            kind: "verified".into(),
            reason: None,
            buffered_at: (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339(),
        };
        write_atomic(&home, &stale).unwrap();

        let now = chrono::Utc::now();
        assert_eq!(sweep_expired(&home, now), 1, "only the 48h-stale entry");
        assert!(
            !drain_for_head(&home, "sha-fresh").is_empty(),
            "the fresh entry survives the sweep"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// A corrupt `kind` reconstructs as `Unverified` (never merge-ready) — a
    /// broken buffer entry can't spuriously flip a PR to mergeable.
    #[test]
    fn corrupt_kind_maps_to_unverified_2059() {
        let v = BufferedVerdict {
            reviewed_head: "s".into(),
            reviewer: "r".into(),
            kind: "garbage".into(),
            reason: None,
            buffered_at: chrono::Utc::now().to_rfc3339(),
        };
        assert!(matches!(
            v.verdict_kind(),
            super::super::VerdictKind::Unverified
        ));
    }
}
