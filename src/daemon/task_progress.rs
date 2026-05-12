//! Sprint 59 Wave 1 PR-1 (#9 task stall watchdog) — per-task
//! progress timestamp sidecar.
//!
//! Scope: tracks the most recent observed signal that work is
//! progressing on an `in_progress` task. The anti-stall scanner
//! ([`crate::daemon::anti_stall`]) reads this sidecar to compute
//! elapsed-time-since-progress against the task's `eta_secs`.
//!
//! Why a sidecar (vs. a field on `TaskRecord`):
//! - Progress is high-frequency telemetry — every `send` call with
//!   `task_id`, every CI commit on a watched branch, every
//!   reviewer verdict. Folding these into the canonical task event
//!   log would bloat replay state without contributing to the
//!   audit trail (the `created` / `claimed` / `in_progress` /
//!   `done` transitions remain the canonical task lifecycle).
//! - Per-task file enables per-task flock for concurrent updates
//!   (multiple hooks can fire near-simultaneously: a broadcast
//!   send + a CI watcher tick).
//! - Decouples Sprint 59 watchdog from the v2 task event schema
//!   contract (Wave 1 PR-2 forward-compat preservation).
//!
//! On-disk shape (`<home>/task-progress/<task_id>.json`):
//! ```json
//! {
//!   "schema_version": 1,
//!   "task_id": "t-...",
//!   "last_progress_at": "2026-05-09T08:45:00.000Z",
//!   "source": "broadcast" | "ci_push" | "ci_verdict"
//! }
//! ```
//!
//! Failure modes (all fail-open per anti-stall design):
//! - Missing dir: lazy-created on first touch.
//! - Read failure / malformed JSON: scanner treats as "no progress
//!   recorded yet" and falls back to `dispatched_at` for elapsed
//!   computation. Stall detection still works; the only loss is
//!   the freshness of the fallback timestamp.
//! - Write failure: silently swallowed via `tracing::warn` —
//!   under-suppressing a stall warning is preferable to crashing
//!   the broadcast dispatch path.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const PROGRESS_DIR: &str = "task-progress";
const SCHEMA_VERSION: u32 = 1;

/// On-disk shape for a single task's progress sidecar. `#[serde(default)]`
/// on each field for forward-compat (Sprint 58 Wave 1 PR-2 contract):
/// a future v2 reader that adds fields can deserialize v1 files cleanly.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ProgressSidecar {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    task_id: String,
    #[serde(default)]
    last_progress_at: String,
    #[serde(default)]
    source: String,
}

/// Identifier for the hook that fired the touch. Surfaces in the
/// sidecar so operator forensics can trace which signal kept a
/// task "fresh" (helps tune ETA estimates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProgressSource {
    /// Hook (a): MCP `send` call carrying a `task_id` arg.
    Broadcast,
    /// Hook (b): CI watcher detected a new commit on a watched
    /// branch correlating to the task.
    CiPush,
    /// Hook (c): MCP `send kind=report` (reviewer verdict) with
    /// `correlation_id` set to the task_id.
    CiVerdict,
}

impl ProgressSource {
    fn as_str(self) -> &'static str {
        match self {
            ProgressSource::Broadcast => "broadcast",
            ProgressSource::CiPush => "ci_push",
            ProgressSource::CiVerdict => "ci_verdict",
        }
    }
}

fn progress_dir(home: &Path) -> PathBuf {
    home.join(PROGRESS_DIR)
}

fn progress_path(home: &Path, task_id: &str) -> PathBuf {
    progress_dir(home).join(format!("{task_id}.json"))
}

/// Touch progress for a task — writes (or overwrites) the sidecar
/// with `last_progress_at = now()` and the supplied `source` tag.
/// Atomic via temp + fsync + rename. Per-task lock prevents
/// concurrent-write torn-state.
///
/// Best-effort: returns silently on IO failure (logged via
/// `tracing::warn`). The anti-stall scanner is fail-open by
/// design — under-suppressing a stall is acceptable, panicking
/// the broadcast dispatch path is not.
pub(crate) fn touch(home: &Path, task_id: &str, source: ProgressSource) {
    if task_id.is_empty() {
        return;
    }
    let dir = progress_dir(home);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, dir = %dir.display(), "task_progress: mkdir failed");
        return;
    }
    let path = progress_path(home, task_id);
    let lock_path = dir.join(format!(".{task_id}.lock"));
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, task = %task_id, "task_progress: flock failed");
            return;
        }
    };
    let payload = ProgressSidecar {
        schema_version: SCHEMA_VERSION,
        task_id: task_id.to_string(),
        last_progress_at: chrono::Utc::now().to_rfc3339(),
        source: source.as_str().to_string(),
    };
    let body = match serde_json::to_string_pretty(&payload) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "task_progress: serialize failed");
            return;
        }
    };
    if let Err(e) = crate::store::atomic_write(&path, body.as_bytes()) {
        tracing::warn!(error = %e, path = %path.display(), "task_progress: write failed");
    }
}

/// Read the last progress timestamp for a task, if recorded.
/// Returns `None` for missing dir, missing file, malformed JSON,
/// or unparseable timestamp — the anti-stall scanner falls back
/// to `dispatched_at` in those cases.
pub(crate) fn read_last_progress_at(
    home: &Path,
    task_id: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let path = progress_path(home, task_id);
    let content = std::fs::read_to_string(&path).ok()?;
    let sidecar: ProgressSidecar = serde_json::from_str(&content).ok()?;
    if sidecar.schema_version != SCHEMA_VERSION {
        // Forward-compat preservation (Sprint 58 Wave 1 PR-2): a
        // future-version sidecar should remain on disk untouched.
        // Skip the read (treat as no progress recorded) — anti-stall
        // falls back to dispatched_at, which is still a valid
        // freshness floor.
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(&sidecar.last_progress_at)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Clear progress sidecar for a task — typically called when the
/// task transitions to a terminal state (Done, Cancelled). Best-
/// effort; missing-file errors are silently OK.
///
/// `#[allow(dead_code)]` rationale: anti-stall scanner only fires
/// for `status=in_progress` tasks, so a stale sidecar from a
/// completed task is ignored automatically. The function is
/// retained as a clean-up surface for future Sprint 60+ task GC
/// pass that may want to reclaim sidecar disk space.
#[allow(dead_code)]
pub(crate) fn clear(home: &Path, task_id: &str) {
    let _ = std::fs::remove_file(progress_path(home, task_id));
}

/// Sprint 59 Wave 1 PR-1 (#9 task stall watchdog) — progress hook
/// (b) PR push via CI watch. Looks up any agent binding whose
/// `branch` matches the supplied branch, extracts the bound
/// `task_id`, and touches its progress sidecar with
/// [`ProgressSource::CiPush`].
///
/// Returns the task_id that was touched (or `None` when no
/// matching binding was found). Best-effort: missing dir / parse
/// failures are silently swallowed via the underlying
/// [`touch`] / sidecar reads.
pub(crate) fn touch_progress_for_branch(home: &Path, branch: &str) -> Option<String> {
    if branch.is_empty() {
        return None;
    }
    let runtime_dir = crate::paths::runtime_dir(home);
    let entries = std::fs::read_dir(&runtime_dir).ok()?;
    for entry in entries.flatten() {
        let binding_path = entry.path().join("binding.json");
        let Ok(content) = std::fs::read_to_string(&binding_path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if v["branch"].as_str() != Some(branch) {
            continue;
        }
        let task_id = v["task_id"].as_str().unwrap_or("");
        // task_id="" means "self" — agent bound itself without a
        // task board entry; no progress to touch.
        if task_id.is_empty() || task_id == "self" {
            continue;
        }
        touch(home, task_id, ProgressSource::CiPush);
        return Some(task_id.to_string());
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-task-progress-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn touch_creates_sidecar_with_current_timestamp() {
        let home = tmp_home("touch-create");
        touch(&home, "t-test-1", ProgressSource::Broadcast);
        let read = read_last_progress_at(&home, "t-test-1");
        assert!(read.is_some(), "sidecar must be readable post-touch");
        let now = chrono::Utc::now();
        let delta = now.signed_duration_since(read.unwrap()).num_seconds();
        assert!(
            (-2..=2).contains(&delta),
            "timestamp must be ~now (delta={delta}s)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn touch_subsequent_overwrites_with_latest_timestamp() {
        let home = tmp_home("touch-overwrite");
        touch(&home, "t-test-2", ProgressSource::Broadcast);
        let first = read_last_progress_at(&home, "t-test-2").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        touch(&home, "t-test-2", ProgressSource::CiPush);
        let second = read_last_progress_at(&home, "t-test-2").unwrap();
        assert!(
            second > first,
            "second touch must produce later timestamp: first={first} second={second}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn touch_with_empty_task_id_is_noop() {
        let home = tmp_home("touch-empty");
        touch(&home, "", ProgressSource::Broadcast);
        // No file created; no panic.
        let dir = progress_dir(&home);
        assert!(
            !dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none(),
            "empty task_id must not create any file"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn read_returns_none_for_missing_sidecar() {
        let home = tmp_home("read-missing");
        let read = read_last_progress_at(&home, "t-never-touched");
        assert!(read.is_none(), "missing sidecar must read as None");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn read_returns_none_for_corrupt_json() {
        let home = tmp_home("read-corrupt");
        let dir = progress_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("t-corrupt.json"), "{not valid json").unwrap();
        let read = read_last_progress_at(&home, "t-corrupt");
        assert!(read.is_none(), "corrupt JSON must read as None (fail-open)");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn read_returns_none_for_future_schema_version() {
        // Forward-compat preservation: future-version sidecar must
        // be left untouched + read returns None (anti-stall falls
        // back to dispatched_at).
        let home = tmp_home("read-forward-version");
        let dir = progress_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = serde_json::json!({
            "schema_version": SCHEMA_VERSION + 1,
            "task_id": "t-future",
            "last_progress_at": "2026-05-09T08:45:00Z",
            "source": "broadcast",
        });
        std::fs::write(
            dir.join("t-future.json"),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        let read = read_last_progress_at(&home, "t-future");
        assert!(
            read.is_none(),
            "future-version sidecar must NOT be read (forward-compat preserved)"
        );
        // File still present (untouched).
        assert!(
            dir.join("t-future.json").exists(),
            "future-version file must remain on disk"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn clear_removes_sidecar() {
        let home = tmp_home("clear");
        touch(&home, "t-test-3", ProgressSource::Broadcast);
        assert!(read_last_progress_at(&home, "t-test-3").is_some());
        clear(&home, "t-test-3");
        assert!(read_last_progress_at(&home, "t-test-3").is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn clear_on_missing_sidecar_is_noop() {
        let home = tmp_home("clear-missing");
        // Doesn't panic.
        clear(&home, "t-never-existed");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn source_tag_preserved_in_sidecar() {
        let home = tmp_home("source-tag");
        touch(&home, "t-tag", ProgressSource::CiVerdict);
        let path = progress_path(&home, "t-tag");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("\"ci_verdict\""),
            "source tag must surface in sidecar: {content}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ─────────────────────────────────────────────────────────────
    // Lead-spec named tests for the 3 progress hooks.
    // ─────────────────────────────────────────────────────────────

    #[test]
    fn progress_hook_broadcast_updates_last_progress_at() {
        // Lead spec name: broadcast send hook.
        let home = tmp_home("hook-broadcast");
        let before = read_last_progress_at(&home, "t-hook-bcast");
        assert!(before.is_none(), "no prior progress");
        touch(&home, "t-hook-bcast", ProgressSource::Broadcast);
        let after = read_last_progress_at(&home, "t-hook-bcast");
        assert!(after.is_some(), "post-touch must be readable");
        let path = progress_path(&home, "t-hook-bcast");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("\"broadcast\""),
            "broadcast source tag must surface: {content}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn progress_hook_pr_push_updates_last_progress_at() {
        // Lead spec name: PR push via CI watch hook. Set up a
        // binding on a branch, then call touch_progress_for_branch
        // — must locate the binding's task_id and touch the
        // sidecar with source=ci_push.
        let home = tmp_home("hook-pr-push");
        // Seed a binding with task_id + branch.
        let runtime_dir = crate::paths::runtime_dir(&home).join("dev");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let binding = serde_json::json!({
            "version": 1,
            "agent": "dev",
            "task_id": "t-hook-pr",
            "branch": "feature/x",
            "issued_at": "2026-05-09T00:00:00Z",
        });
        std::fs::write(
            runtime_dir.join("binding.json"),
            serde_json::to_string_pretty(&binding).unwrap(),
        )
        .unwrap();
        let touched = touch_progress_for_branch(&home, "feature/x");
        assert_eq!(
            touched.as_deref(),
            Some("t-hook-pr"),
            "must look up task_id from matching binding: {touched:?}"
        );
        let after = read_last_progress_at(&home, "t-hook-pr");
        assert!(after.is_some(), "sidecar must be touched post-lookup");
        let content = std::fs::read_to_string(progress_path(&home, "t-hook-pr")).unwrap();
        assert!(
            content.contains("\"ci_push\""),
            "ci_push source tag must surface: {content}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn progress_hook_ci_verdict_updates_last_progress_at() {
        // Lead spec name: reviewer kind=report verdict hook. Same
        // touch surface as broadcast but with CiVerdict source tag.
        let home = tmp_home("hook-ci-verdict");
        touch(&home, "t-hook-verdict", ProgressSource::CiVerdict);
        let after = read_last_progress_at(&home, "t-hook-verdict");
        assert!(after.is_some());
        let content = std::fs::read_to_string(progress_path(&home, "t-hook-verdict")).unwrap();
        assert!(
            content.contains("\"ci_verdict\""),
            "ci_verdict source tag must surface: {content}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn touch_progress_for_branch_returns_none_when_no_binding_matches() {
        // Defensive: empty/missing binding → None, no panic, no
        // sidecar created.
        let home = tmp_home("hook-no-binding");
        let touched = touch_progress_for_branch(&home, "feature/never-bound");
        assert!(touched.is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn touch_progress_for_branch_skips_self_bindings() {
        // Defensive: bind_self uses task_id="self" as a marker.
        // touch_progress_for_branch must skip these — they have no
        // task board entry to track progress against.
        let home = tmp_home("hook-self-binding");
        let runtime_dir = crate::paths::runtime_dir(&home).join("dev");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let binding = serde_json::json!({
            "version": 1,
            "agent": "dev",
            "task_id": "self",
            "branch": "feature/y",
            "issued_at": "2026-05-09T00:00:00Z",
        });
        std::fs::write(
            runtime_dir.join("binding.json"),
            serde_json::to_string_pretty(&binding).unwrap(),
        )
        .unwrap();
        let touched = touch_progress_for_branch(&home, "feature/y");
        assert!(
            touched.is_none(),
            "self-binding must NOT trigger progress touch"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
