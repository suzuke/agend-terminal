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
/// the rewrite never runs). Phase 1 sealed the *duplicate* vector; #2168 Phase 2
/// adds (a) an existence-guarded repair re-append ([`record_task_project_if_absent`])
/// so duplicates no longer accumulate between compactions, and (b) *orphan*
/// pruning (entries for tasks deleted from every board) folded into the same
/// gated compaction via [`live_task_ids`].
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
    let mut kept = dedup_entries(&entries);
    // #2168 Phase-2: orphan-prune — drop entries whose task no longer exists on
    // ANY board. This is the slow-growth vector Phase-1 (dedup) does NOT address:
    // a long-lived, high-task-volume daemon accumulates deleted-task entries that
    // never dedup away, and once the index exceeds the threshold EVERY append
    // re-reads + re-parses the whole file (a real per-append O(n) cost). Pruning
    // orphans shrinks the index back below the threshold, restoring O(1) appends.
    // FAIL-SAFE: `live_task_ids` returns None if ANY board's replay errors, so a
    // transiently-unreadable board can never be mistaken for "its tasks were all
    // deleted" and false-prune a LIVE task's entry (the highest-risk failure).
    if let Some(live) = live_task_ids(home) {
        kept.retain(|e| live.contains(&e.task_id));
    }
    if kept.len() == raw_line_count {
        // Nothing removed (no duplicates, no orphans, no corrupt lines) → a
        // rewrite would be a no-op.
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
        "compacted task_index.jsonl (deduped + orphan-pruned append-only growth)"
    );
    Ok(())
}

/// The set of task_ids live on SOME board across ALL projects — the live set for
/// #2168 Phase-2 orphan-prune. Returns `None` (→ caller SKIPS pruning, dedup
/// only) if ANY board's replay errors, so a transient read failure of one board
/// can never be misread as "every task on it was deleted" and false-prune live
/// entries. A board that simply holds no tasks replays to an empty state (`Ok`)
/// and contributes nothing; only a corrupt/unreadable EXISTING board yields
/// `Err`. Covers EVERY board via `enumerate_projects` — missing a board would
/// false-orphan its live tasks, so completeness here is load-bearing.
fn live_task_ids(home: &Path) -> Option<std::collections::HashSet<String>> {
    let mut live = std::collections::HashSet::new();
    for project in enumerate_projects(home) {
        let state = crate::task_events::replay_at(&board_root(home, &project)).ok()?;
        for tid in state.tasks.keys() {
            live.insert(tid.0.clone());
        }
    }
    Some(live)
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

/// #2168 Phase-2a: the index-repair counterpart of [`record_task_project`] —
/// append ONLY if `task_id` is not already indexed, with the existence check and
/// the append performed atomically under the SAME `task_index.jsonl.lock`.
///
/// The plain `record_task_project` re-append in [`resolve_task_project`] was
/// unconditional, so two resolves racing on a not-yet-indexed task (or a resolve
/// after a transient index loss) could each append a duplicate (TOCTOU). The
/// caller's `lookup_task_project` is unlocked and cannot close that window; this
/// re-checks INSIDE `append_lines_under_lock`'s `build_lines` closure (which runs
/// while the lock is held), so the first writer wins and the rest are no-ops.
pub(super) fn record_task_project_if_absent(
    home: &Path,
    task_id: &str,
    project_id: &str,
) -> anyhow::Result<()> {
    let entry = serde_json::to_string(&IndexEntry {
        task_id: task_id.to_string(),
        project_id: project_id.to_string(),
    })?;
    // `build_lines` runs under the task_index lock: an empty Vec means "already
    // present → append nothing". No nested lock (the closure does not re-acquire).
    crate::event_log::append_lines_under_lock(home, "task_index", |log_path| {
        if index_contains(log_path, task_id) {
            Ok(vec![])
        } else {
            Ok(vec![entry])
        }
    })?;
    maybe_compact_index(home);
    Ok(())
}

/// True if `task_index.jsonl` already carries an entry for `task_id`. A missing
/// file → false; torn/corrupt lines are skipped (same fail-safe as
/// [`lookup_task_project`]'s `find_map`).
fn index_contains(index_path: &Path, task_id: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(index_path) else {
        return false;
    };
    content.lines().any(|l| {
        serde_json::from_str::<IndexEntry>(l)
            .ok()
            .is_some_and(|e| e.task_id == task_id)
    })
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
            // Repair the index so the next lookup is O(1). #2168 Phase-2a: use
            // the absent-guarded variant so a concurrent repair (or a repair
            // after a transient index loss) cannot append a duplicate entry —
            // the existence check is atomic under the index lock.
            let _ = record_task_project_if_absent(home, task_id, &project);
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

    fn index_lines(home: &Path) -> usize {
        std::fs::read_to_string(index_path(home))
            .map(|c| c.lines().count())
            .unwrap_or(0)
    }

    /// Seed a real (live) task on `project`'s board so #2168 Phase-2 orphan-prune
    /// treats its index entry as live. `DEFAULT_PROJECT` → the home board.
    fn seed_live_task(home: &Path, project: &str, task_id: &str) {
        use crate::task_events::{append_batch_at, InstanceName, TaskEvent};
        append_batch_at(
            &board_root(home, project),
            &InstanceName::from("test:seed"),
            vec![TaskEvent::Created {
                task_id: TaskId(task_id.to_string()),
                title: "t".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            }],
        )
        .expect("seed live task");
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
        // #2168 Phase-2: seed both tasks LIVE so orphan-prune keeps them and this
        // test isolates the dedup behaviour (an unseeded entry would be pruned).
        seed_live_task(&home, "proj-a", "T-dup");
        seed_live_task(&home, "proj-b", "T-other");
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
        // #2168 Phase-2: seed T-1 LIVE so orphan-prune keeps it; this test
        // isolates corrupt-line + dedup behaviour.
        seed_live_task(&home, "proj-a", "T-1");
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

    // ── #2168 Phase-2 (behavioral; replaces the static review_tasks_10 scan) ──

    /// Phase-2a (CR #235 / review_tasks_10): the index-repair re-append is
    /// guarded — `record_task_project_if_absent` is idempotent. The pre-fix
    /// repair used `record_task_project` UNCONDITIONALLY, so a concurrent repair
    /// or a repair after index loss re-appended a duplicate. Behavioral (drives
    /// the real append + reads the file on disk), not a body source-scan — the
    /// honest fix lives in a helper, so a body-pinned static scan could only pass
    /// via a comment-satisfiable needle (#2018/#2206/t-16 anti-pattern).
    #[test]
    fn record_if_absent_is_idempotent_and_keeps_first_project() {
        let home = tmp_home("if-absent");
        record_task_project_if_absent(&home, "T", "proj-a").unwrap();
        // Second call (same task, DIFFERENT project) must NOT append: the
        // existence check is atomic under the index lock.
        record_task_project_if_absent(&home, "T", "proj-WRONG").unwrap();
        assert_eq!(
            index_lines(&home),
            1,
            "absent-guard must not re-append an indexed task"
        );
        assert_eq!(
            lookup_task_project(&home, "T").as_deref(),
            Some("proj-a"),
            "first recorded project preserved"
        );
        // Control: the UNGUARDED record_task_project appends unconditionally —
        // the duplicate-growth vector record_if_absent closes.
        record_task_project(&home, "T", "proj-a").unwrap();
        assert_eq!(
            index_lines(&home),
            2,
            "control: unguarded record appends a duplicate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Phase-2a (integration): a `resolve_task_project` repair, then a re-resolve
    /// after the index is lost, never accumulates duplicates — the repair routes
    /// through the absent-guarded variant.
    #[test]
    fn resolve_repair_is_idempotent_across_index_loss() {
        let home = tmp_home("repair-idem");
        seed_live_task(&home, DEFAULT_PROJECT, "T-r");
        assert_eq!(resolve_task_project(&home, "T-r"), DEFAULT_PROJECT);
        assert_eq!(
            index_lines(&home),
            1,
            "first resolve repairs the index once"
        );
        // Re-resolve with the entry already present must not append again.
        assert_eq!(resolve_task_project(&home, "T-r"), DEFAULT_PROJECT);
        assert_eq!(
            index_lines(&home),
            1,
            "re-resolve does not duplicate the repair"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Phase-2b: over-threshold compaction prunes ORPHAN entries (tasks that no
    /// longer exist on any board) while keeping LIVE entries and preserving their
    /// resolution. Orphans are the slow-growth vector Phase-1 dedup never reached.
    #[test]
    fn compaction_prunes_orphan_entries_over_threshold() {
        let home = tmp_home("orphan-prune");
        seed_live_task(&home, DEFAULT_PROJECT, "T-live");
        record_task_project(&home, "T-live", DEFAULT_PROJECT).unwrap();
        // An orphan: an index entry for a task present on NO board.
        record_task_project(&home, "T-orphan", "proj-gone").unwrap();
        compact_index_gated(&home, 1, u64::MAX).unwrap();
        assert_eq!(index_lines(&home), 1, "orphan pruned, live entry kept");
        assert_eq!(
            lookup_task_project(&home, "T-live").as_deref(),
            Some(DEFAULT_PROJECT),
            "live task's resolution preserved"
        );
        assert_eq!(
            lookup_task_project(&home, "T-orphan"),
            None,
            "orphan no longer resolvable via the index"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Phase-2b ⚠ highest-risk path: a task LIVE on a NON-default board must NOT
    /// be false-orphaned — `live_task_ids` must cover EVERY board, not just the
    /// home/default board. Missing a board here would silently drop live entries.
    #[test]
    fn compaction_keeps_live_task_on_non_default_board() {
        let home = tmp_home("orphan-multiboard");
        seed_live_task(&home, "proj-x", "T-x"); // live on a non-default board
        record_task_project(&home, "T-x", "proj-x").unwrap();
        record_task_project(&home, "T-orphan", "proj-gone").unwrap(); // forces a rewrite
        compact_index_gated(&home, 1, u64::MAX).unwrap();
        assert_eq!(
            lookup_task_project(&home, "T-x").as_deref(),
            Some("proj-x"),
            "a task live on a non-default board must not be false-orphaned"
        );
        assert_eq!(
            lookup_task_project(&home, "T-orphan"),
            None,
            "true orphan pruned"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Phase-2b fail-safe: if ANY board's replay errors, `live_task_ids` returns
    /// None so orphan-prune is SKIPPED entirely — a transiently-unreadable board
    /// can never be misread as "its tasks were deleted" and false-prune live
    /// entries. Force the error by making a board's event log a directory.
    #[test]
    fn live_task_ids_is_none_when_a_board_replay_fails() {
        let home = tmp_home("livetids-failsafe");
        seed_live_task(&home, DEFAULT_PROJECT, "T-live");
        // A board whose event log (`task_events.jsonl`) is a DIRECTORY → replay's
        // read_to_string errors → live_task_ids must bail to None.
        let bad = board_root(&home, "proj-bad");
        std::fs::create_dir_all(bad.join("task_events.jsonl")).unwrap();
        assert!(
            live_task_ids(&home).is_none(),
            "a board whose replay errors must yield None so orphan-prune is skipped"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
