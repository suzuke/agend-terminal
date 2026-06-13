//! #2059 #2(c): the verdict buffer — a TTL-bounded sidecar that holds a review
//! verdict which arrived BEFORE its pr-state existed (the verdict-before-CI
//! ordering gap that caused the #2058 dead zone).
//!
//! [`super::record_verdict`] now keys on the verdict's `reviewed_head` (the SHA
//! the reviewer asserts they reviewed) instead of the task→branch chain. When no
//! pr-state for that SHA exists yet, the verdict is buffered here keyed by the
//! SHA; [`super::record_ci_result`] drains + replays matching buffered verdicts
//! the moment it creates/observes a branch state at that head. This is the #1888
//! *track-until-resolution* pattern: a signal that precedes its consumer is
//! persisted and replayed on the resolving event, never dropped on the floor.
//!
//! Storage: file-per-entry under
//! `<home>/pr-state/verdict-buffer/<sha>--<reviewer>.json`, atomic write
//! (`tmp`+`rename`), 24 h TTL — a SHA that never becomes any branch's head
//! self-expires so the buffer can't leak.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A SHA that never resolves to a branch head is dropped after this. 24 h
/// matches the `ci_handoff_track` TTL (#1888) — the same orphan-signal horizon.
const TTL_HOURS: i64 = 24;

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
            "#2059 verdict_buffer: buffered verdict (no pr-state at this SHA yet — replays on CI observe)"
        ),
        Err(e) => tracing::warn!(reviewed_head, reviewer, error = %e, "#2059 verdict_buffer: write failed"),
    }
}

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

/// Drain (read **and delete**) every buffered verdict whose `reviewed_head`
/// matches `head`. Called by [`super::record_ci_result`] the moment a pr-state's
/// head_sha is established at `head`, so the replayed verdicts land on the
/// just-created/observed state. A SHA mismatch leaves the entry parked (a future
/// CI observation at that SHA, or the TTL sweep, resolves it).
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
    let dir = buffer_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_buffer_file(&path) {
            continue;
        }
        let expired = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<BufferedVerdict>(&c).ok())
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(&v.buffered_at).ok())
            .map(|t| {
                now.signed_duration_since(t.with_timezone(&chrono::Utc))
                    > chrono::Duration::hours(TTL_HOURS)
            })
            // Unparseable / unreadable → a broken entry must not linger forever.
            .unwrap_or(true);
        if expired {
            let _ = std::fs::remove_file(&path);
            removed += 1;
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
