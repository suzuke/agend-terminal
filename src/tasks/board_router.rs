//! #2117 P1 BoardRouter — the single resolution path from a caller or a task_id
//! to the on-disk board it belongs to.
//!
//! P0 (#2119) added the `board_root` storage seam (every `task_events` storage
//! fn has an `_at(board, …)` variant). P1 decides WHICH board each `task`
//! command targets:
//!
//! - [`resolve_current_project`] maps a caller → its team's `source_repo` →
//!   project id (used by `create` and the `list` default).
//! - [`resolve_task_project`] maps a task_id → project id via the append-only
//!   `task_index.jsonl` (a task never changes project, so the index is
//!   immutable), with a full-board-scan fallback that repairs a missing entry.
//!
//! **Single-project byte-identical:** a deployment with no per-team
//! `source_repo` resolves every caller/task to [`DEFAULT_PROJECT`], whose
//! `board_root` IS `home` — so the index holds one project, `list` defaults to
//! that one board (= the whole board), and every path/behaviour matches pre-P1.
//!
//! A **project id is itself the filesystem-safe slug** (so the board directory
//! name equals the project id → reversible for enumeration / the fallback
//! scan). `board_root(home, project_id)` is idempotent on an already-safe id.

use crate::task_events::{board_root, project_slug, TaskId, DEFAULT_PROJECT};
use std::path::{Path, PathBuf};

use super::Task;

// ── task_index (home/task_index.jsonl) ─────────────────────────────

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct IndexEntry {
    task_id: String,
    project_id: String,
}

fn index_path(home: &Path) -> PathBuf {
    home.join("task_index.jsonl")
}

/// #2135 R4 Phase 1: `task_index.jsonl` is append-only, so the
/// `resolve_task_project` repair re-append can write a duplicate entry for a
/// task_id already in the index, and the file grows without bound. Compaction
/// is **size-gated lazy**: `record_task_project` calls [`maybe_compact_index`]
/// after each append, but it only rewrites the file once it crosses a
/// conservative threshold (so a small index is byte-identical to pre-#2135 —
/// the rewrite never runs). Phase 1 seals only the *duplicate* vector; orphan
/// pruning (entries for deleted tasks) is deferred to Phase 2.
const COMPACT_LINE_THRESHOLD: usize = 2000;
const COMPACT_BYTE_THRESHOLD: u64 = 256 * 1024;

/// Keep the FIRST entry per task_id, preserving order. Mirrors
/// [`lookup_task_project`]'s first-match semantics exactly, so compaction is
/// resolution-preserving: a lookup before and after compaction returns the same
/// project for every task_id. A pure fn over parsed entries — unit-testable
/// without touching the filesystem.
fn dedup_entries(entries: &[IndexEntry]) -> Vec<IndexEntry> {
    let mut seen = std::collections::HashSet::new();
    entries
        .iter()
        .filter(|e| seen.insert(e.task_id.clone()))
        .cloned()
        .collect()
}

/// Best-effort, size-gated compaction of `task_index.jsonl`. Runs after a
/// `record_task_project` append; a failure never propagates to the record path
/// (the append already succeeded — compaction is opportunistic cleanup).
fn maybe_compact_index(home: &Path) {
    if let Err(e) = compact_index_gated(home, COMPACT_LINE_THRESHOLD, COMPACT_BYTE_THRESHOLD) {
        tracing::warn!(error = %e, "task_index compaction failed (non-fatal; append already durable)");
    }
}

/// The gated compaction core (thresholds injected for testability). Holds the
/// SAME `task_index.jsonl.lock` as [`crate::event_log::append_lines_under_lock`]
/// (the append path) so concurrent appenders never observe a half-rewritten
/// file. No `api::call` runs under the lock (#1629). Write-back mirrors
/// `task_events`' tmp + fsync + atomic-rename so a crash mid-rewrite leaves the
/// original file intact.
fn compact_index_gated(
    home: &Path,
    line_threshold: usize,
    byte_threshold: u64,
) -> anyhow::Result<()> {
    let path = index_path(home);
    let lock_path = path.with_extension("jsonl.lock");
    let _lock = crate::store::acquire_file_lock(&lock_path)?;

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        // Missing index → nothing to compact.
        Err(_) => return Ok(()),
    };
    let raw_line_count = content.lines().count();
    let over_threshold = raw_line_count > line_threshold || content.len() as u64 > byte_threshold;
    if !over_threshold {
        // Below threshold → byte-identical to pre-#2135 (no rewrite).
        return Ok(());
    }

    // Parse good lines (skip torn/corrupt lines — same fail-safe as
    // `lookup_task_project`'s `find_map`, which already tolerates them).
    let entries: Vec<IndexEntry> = content
        .lines()
        .filter_map(|l| serde_json::from_str::<IndexEntry>(l).ok())
        .collect();
    let kept = dedup_entries(&entries);
    if kept.len() == raw_line_count {
        // No duplicates and no corrupt lines to drop → a rewrite would be a
        // no-op (Phase 1 targets duplicates only; orphan growth is Phase 2).
        return Ok(());
    }

    // tmp + fsync + atomic rename (mirrors task_events compaction write-back).
    use std::io::Write;
    let tmp = path.with_extension("jsonl.tmp");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)?;
    for e in &kept {
        writeln!(f, "{}", serde_json::to_string(e)?)?;
    }
    f.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    tracing::info!(
        before = raw_line_count,
        after = kept.len(),
        "compacted task_index.jsonl (deduped append-only growth)"
    );
    Ok(())
}

/// Record a task's project at create time. Append-only under the shared
/// file-lock; a task never moves project, so the first entry for a task_id is
/// authoritative and a stray duplicate is harmless ([`lookup_task_project`]
/// takes the first match).
pub(super) fn record_task_project(
    home: &Path,
    task_id: &str,
    project_id: &str,
) -> anyhow::Result<()> {
    let line = serde_json::to_string(&IndexEntry {
        task_id: task_id.to_string(),
        project_id: project_id.to_string(),
    })?;
    // `append_lines_under_lock(home, "task_index", …)` writes the same
    // `home/task_index.jsonl` as `index_path`, under `<file>.jsonl.lock`.
    crate::event_log::append_lines_under_lock(home, "task_index", |_p| Ok(vec![line]))?;
    // #2135 R4 Phase 1: size-gated lazy compaction seals the append-only
    // duplicate-growth vector. Sequential to (not nested under) the append's
    // lock — best-effort, never fails the record.
    maybe_compact_index(home);
    Ok(())
}

fn lookup_task_project(home: &Path, task_id: &str) -> Option<String> {
    let content = std::fs::read_to_string(index_path(home)).ok()?;
    content.lines().find_map(|line| {
        serde_json::from_str::<IndexEntry>(line)
            .ok()
            .filter(|e| e.task_id == task_id)
            .map(|e| e.project_id)
    })
}

// ── project resolution ─────────────────────────────────────────────

/// Derive a stable, filesystem-safe project id from a team's `source_repo`.
/// Uses the trailing `owner/repo` segments (`.git` stripped) when present, then
/// slugs to a safe directory name (the same `project_slug` `board_root` applies,
/// so the result is idempotent under `board_root`).
pub(crate) fn project_id_from_source_repo(repo: &Path) -> String {
    let segs: Vec<String> = repo
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str().map(|x| x.to_string()),
            _ => None,
        })
        .collect();
    let strip_git = |s: &str| s.strip_suffix(".git").unwrap_or(s).to_string();
    let raw = match segs.len() {
        0 => repo.to_string_lossy().into_owned(),
        1 => strip_git(&segs[0]),
        n => format!("{}/{}", segs[n - 2], strip_git(&segs[n - 1])),
    };
    project_slug(&raw)
}

/// The project a caller currently acts in: its team's `source_repo`, else the
/// fleet-wide [`DEFAULT_PROJECT`]. (No team / no `source_repo` → default → the
/// `home` board → byte-identical for single-project deployments.)
pub(super) fn resolve_current_project(home: &Path, caller: &str) -> String {
    crate::teams::find_team_for(home, caller)
        .and_then(|t| t.source_repo)
        .map(|repo| project_id_from_source_repo(&repo))
        .unwrap_or_else(|| DEFAULT_PROJECT.to_string())
}

/// #2117 P3a (reviewer-4 #2133 finding): the fail-closed counterpart of
/// [`resolve_current_project`] for the per-board ACL. `resolve_current_project`
/// collapses a HARD fleet.yaml read/parse failure into [`DEFAULT_PROJECT`] — right
/// for `create`/`list` board routing (a missing/unreadable fleet just means
/// single-project → the `home` board), but WRONG for authorization: an ACL that
/// reads DEFAULT on a fleet read error would fail-OPEN to the default board.
///
/// This distinguishes the two cases the plain resolver conflates, mirroring the
/// #1744-M7 three-state [`crate::teams::try_load_fleet`] (missing file =
/// `Ok(default)`, file present but unreadable/corrupt = `Err`):
/// - hard read/parse failure → `Err` → the ACL denies (fail-closed);
/// - a *legitimate* no-team / no-`source_repo` caller → `Ok(DEFAULT_PROJECT)` →
///   the ACL still allows on the default board, so single-project stays
///   byte-identical (no new denial).
pub(super) fn resolve_current_project_checked(home: &Path, caller: &str) -> anyhow::Result<String> {
    let fleet = crate::teams::try_load_fleet(home)?;
    Ok(crate::teams::find_team_for_in(&fleet, caller)
        .and_then(|t| t.source_repo)
        .map(|repo| project_id_from_source_repo(&repo))
        .unwrap_or_else(|| DEFAULT_PROJECT.to_string()))
}

/// The project a dispatch **target** acts in — identical
/// agent→team→`source_repo`→project resolution as [`resolve_current_project`],
/// but keyed on the dispatch target rather than the caller. The comms
/// auto-create path (#2117 P2) stamps this so the spawned task lands on the
/// TARGET's board, not the dispatcher's (P1's `create` defaulted to the
/// *caller's* project — the leak the epic flagged at `comms.rs`). Single-project
/// → [`DEFAULT_PROJECT`] → the `home` board → byte-identical.
pub(crate) fn resolve_target_project(home: &Path, target: &str) -> String {
    resolve_current_project(home, target)
}

/// The project a task lives in: the `task_index` entry, else a full-board scan
/// that repairs the index on a hit, else [`DEFAULT_PROJECT`] (the `home` board)
/// when the task is unknown — which keeps a missing/legacy task resolving to the
/// historical board.
pub(crate) fn resolve_task_project(home: &Path, task_id: &str) -> String {
    if let Some(p) = lookup_task_project(home, task_id) {
        return p;
    }
    let tid = TaskId(task_id.to_string());
    for project in enumerate_projects(home) {
        let found = crate::task_events::replay_at(&board_root(home, &project))
            .map(|state| state.tasks.contains_key(&tid))
            .unwrap_or(false);
        if found {
            // Repair the index so the next lookup is O(1).
            let _ = record_task_project(home, task_id, &project);
            return project;
        }
    }
    DEFAULT_PROJECT.to_string()
}

/// Every project with an on-disk board: the default (fleet) project plus each
/// `home/boards/<project_id>` subdir (the dir name IS the project id).
fn enumerate_projects(home: &Path) -> Vec<String> {
    let mut out = vec![DEFAULT_PROJECT.to_string()];
    if let Ok(entries) = std::fs::read_dir(home.join("boards")) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                if let Some(name) = e.file_name().to_str() {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

// ── board handles + cross-board listing ────────────────────────────

/// Board root for an existing task (via [`resolve_task_project`]).
pub(super) fn board_for_task(home: &Path, task_id: &str) -> PathBuf {
    board_root(home, &resolve_task_project(home, task_id))
}

/// All tasks across every board, tagged with their project id — for the
/// `list project=all` / `scope=fleet` aggregate view.
pub(super) fn list_all_boards(home: &Path) -> Vec<(String, Vec<Task>)> {
    enumerate_projects(home)
        .into_iter()
        .map(|project| {
            let tasks = super::list_all_at(&board_root(home, &project));
            (project, tasks)
        })
        .collect()
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "agend-board-router-{}-{}-{tag}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn entry(t: &str, p: &str) -> IndexEntry {
        IndexEntry {
            task_id: t.to_string(),
            project_id: p.to_string(),
        }
    }

    /// Pure dedup: keep the FIRST entry per task_id, preserving order — exactly
    /// mirroring `lookup_task_project`'s first-match.
    #[test]
    fn dedup_entries_keeps_first_per_task_id_in_order() {
        let input = vec![
            entry("t1", "p1"),
            entry("t1", "pX"),
            entry("t2", "p2"),
            entry("t1", "pY"),
            entry("t3", "p3"),
        ];
        let got: Vec<(String, String)> = dedup_entries(&input)
            .into_iter()
            .map(|e| (e.task_id, e.project_id))
            .collect();
        assert_eq!(
            got,
            vec![
                ("t1".to_string(), "p1".to_string()),
                ("t2".to_string(), "p2".to_string()),
                ("t3".to_string(), "p3".to_string()),
            ],
            "first project per task_id wins, order preserved"
        );
    }

    /// File-level: an append-only index with duplicate entries, once over the
    /// threshold, compacts to one line per task_id — and lookup still resolves
    /// to the FIRST recorded project (resolution-preserving). RED pre-#2135 (no
    /// compaction → all N lines remain).
    #[test]
    fn compact_index_dedups_over_threshold_and_preserves_lookup() {
        let home = tmp_home("compact-over");
        // First record wins; later "repair re-append" duplicates carry a WRONG
        // project to prove dedup keeps the first, not the last.
        record_task_project(&home, "T-dup", "proj-a").unwrap();
        for _ in 0..4 {
            record_task_project(&home, "T-dup", "proj-WRONG").unwrap();
        }
        record_task_project(&home, "T-other", "proj-b").unwrap();
        // Under the real 2000-line threshold the appends did NOT compact.
        let raw = std::fs::read_to_string(index_path(&home)).unwrap();
        assert_eq!(raw.lines().count(), 6, "pre-compaction keeps every append");

        // Force compaction with a tiny line threshold (real threshold is 2000).
        compact_index_gated(&home, 2, u64::MAX).unwrap();

        let after = std::fs::read_to_string(index_path(&home)).unwrap();
        assert_eq!(
            after.lines().count(),
            2,
            "compaction collapses duplicates to one entry per task_id"
        );
        assert_eq!(
            lookup_task_project(&home, "T-dup").as_deref(),
            Some("proj-a"),
            "lookup still returns the FIRST recorded project after compaction"
        );
        assert_eq!(
            lookup_task_project(&home, "T-other").as_deref(),
            Some("proj-b")
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Below threshold the file is byte-identical (no rewrite), even with a
    /// duplicate present — guarantees single-project deployments stay
    /// byte-identical to pre-#2135.
    #[test]
    fn compact_index_below_threshold_is_byte_identical() {
        let home = tmp_home("compact-under");
        record_task_project(&home, "T-1", "proj-a").unwrap();
        record_task_project(&home, "T-1", "proj-a").unwrap(); // a duplicate
        let before = std::fs::read_to_string(index_path(&home)).unwrap();
        // High thresholds → under threshold → must NOT touch the file.
        compact_index_gated(&home, 10_000, u64::MAX).unwrap();
        let after = std::fs::read_to_string(index_path(&home)).unwrap();
        assert_eq!(
            before, after,
            "below threshold must be byte-identical (no rewrite)"
        );
        assert_eq!(
            after.lines().count(),
            2,
            "duplicate retained below threshold"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Fail-safe: a torn/corrupt line is dropped by compaction (same tolerance
    /// `lookup_task_project`'s `find_map` already has), and good entries survive.
    #[test]
    fn compact_index_drops_corrupt_lines_over_threshold() {
        let home = tmp_home("compact-corrupt");
        record_task_project(&home, "T-1", "proj-a").unwrap();
        record_task_project(&home, "T-1", "proj-a").unwrap(); // dup
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(index_path(&home))
                .unwrap();
            writeln!(f, "{{not valid json").unwrap();
        }
        compact_index_gated(&home, 1, u64::MAX).unwrap();
        let after = std::fs::read_to_string(index_path(&home)).unwrap();
        assert_eq!(
            after.lines().count(),
            1,
            "compaction drops the corrupt line and dedups the good ones"
        );
        assert_eq!(lookup_task_project(&home, "T-1").as_deref(), Some("proj-a"));
        std::fs::remove_dir_all(&home).ok();
    }
}
