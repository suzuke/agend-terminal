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

#[derive(serde::Serialize, serde::Deserialize)]
struct IndexEntry {
    task_id: String,
    project_id: String,
}

fn index_path(home: &Path) -> PathBuf {
    home.join("task_index.jsonl")
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
pub(super) fn resolve_task_project(home: &Path, task_id: &str) -> String {
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
