//! BoardRouter — resolves callers to boards and task IDs through the
//! authoritative bounded task catalog.
//!
//! P0 (#2119) added the `board_root` storage seam (every `task_events` storage
//! fn has an `_at(board, …)` variant). P1 decides WHICH board each `task`
//! command targets:
//!
//! - [`resolve_current_project`] maps a caller → its team's `source_repo` →
//!   project id (used by `create` and the `list` default).
//! - [`route_task`] maps a task id through the catalog's global uniqueness index.
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

use super::{Task, TaskRouteError};

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

/// #2509: a team's resolved project id — explicit `project_id` override
/// (slugged for the same filesystem/path-escape safety as the source_repo
/// derivation) takes priority over the `source_repo`-derived guess.
/// `project_id_from_source_repo` guesses `owner/repo` from the last two path
/// segments, which mis-slugs when the local clone sits under an intermediate
/// directory (e.g. `~/Projects/x` → `Projects_x`); the override lets an
/// operator align the team with wherever its tasks actually live without
/// touching `source_repo` (worktree/dispatch identity) or any event history.
/// `None` when the team has set neither, so the caller falls back to
/// [`DEFAULT_PROJECT`] — byte-identical to pre-#2509 for every team that
/// doesn't set `project_id`.
fn project_id_for_team(team: &crate::teams::Team) -> Option<String> {
    team.project_id
        .as_ref()
        .map(|pid| project_slug(pid))
        .or_else(|| team.source_repo.as_deref().map(project_id_from_source_repo))
}

/// The project a caller currently acts in: its team's `project_id` override or
/// `source_repo`-derived guess, else the fleet-wide [`DEFAULT_PROJECT`]. (No
/// team / neither set → default → the `home` board → byte-identical for
/// single-project deployments.)
pub(super) fn resolve_current_project(home: &Path, caller: &str) -> String {
    crate::teams::find_team_for(home, caller)
        .and_then(|t| project_id_for_team(&t))
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
        .and_then(|t| project_id_for_team(&t))
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

// #2760: the lenient `resolve_task_project` (index → scan → DEFAULT fallback) is
// GONE — every per-id authority path now routes through [`route_task`], which
// fails closed (NotFound / Unreadable / Ambiguous) instead of silently defaulting.

/// Every project with an on-disk board: the default (fleet) project plus each
/// `home/boards/<project_id>` subdir (the dir name IS the project id).
///
/// AUDIT2-014: `pub(super)` (was private) so the cross-board dep detective
/// (`tasks::reconcile_stale_cross_board_claims`) can enumerate boards and
/// replay each RAW (`task_events::replay_at`, no in-memory dep derivation) —
/// unlike `list_all_boards`, which applies list-time dep derivation and would
/// silently relabel an InProgress task's persisted status to `Blocked` before
/// the detective ever sees it.
///
/// #2760 R1: FALLIBLE. A MISSING `boards/` dir is legacy/single-project →
/// `Ok([DEFAULT])`. But a `boards/` dir that EXISTS yet cannot be fully
/// enumerated — a `read_dir` I/O error, an unreadable directory entry, an entry
/// whose file-type can't be stat'd, or a non-UTF-8 (thus un-sluggable) board name
/// — makes the project-board set UNPROVABLE → `TaskRouteError::Unreadable`. This
/// closes the fail-OPEN hole where a swallowed enumeration error would let a task
/// living on a project board mis-resolve to `NotFound`/default. Entry errors are
/// NEVER flattened away.
pub(super) fn enumerate_projects(home: &Path) -> Result<Vec<String>, TaskRouteError> {
    let mut out = vec![DEFAULT_PROJECT.to_string()];
    let boards = home.join("boards");
    let unreadable = |cause: String| TaskRouteError::Unreadable {
        path: boards.clone(),
        cause,
    };
    let entries = match std::fs::read_dir(&boards) {
        Ok(entries) => entries,
        // No boards/ dir → single-project/legacy → default board only.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(unreadable(format!("boards dir read_dir failed: {e}"))),
    };
    for entry in entries {
        let entry = entry.map_err(|e| unreadable(format!("boards dir entry unreadable: {e}")))?;
        let file_type = entry
            .file_type()
            .map_err(|e| unreadable(format!("boards dir entry file_type unreadable: {e}")))?;
        if !file_type.is_dir() {
            continue;
        }
        match entry.file_name().to_str() {
            Some(name) => out.push(name.to_string()),
            // A board dir whose name is not UTF-8 can't be slugged/enumerated, yet
            // it EXISTS — it might hold the id, so uniqueness is unprovable.
            None => {
                return Err(unreadable(
                    "boards dir contains a non-UTF-8 board name".to_string(),
                ))
            }
        }
    }
    Ok(out)
}

// ── board handles + cross-board listing ────────────────────────────

// ── #2760 item 2: per-task-ID router lock ──────────────────────────

/// The per-task-ID router lock file. **Board-independent** — it lives at
/// `home/task_locks/<slug>.lock`, not under any board, because it is acquired
/// BEFORE the strict route resolves which board holds the id. Every authority
/// mutation on one id serialises on this lock regardless of which board the task
/// lives on, so the route→revalidate→append transaction is atomic against a
/// concurrent create/mutation of the same id on ANY board.
///
/// The id is slugged for filesystem safety; a real `t-…` task id is already
/// slug-safe (the slug is the identity map on it), so distinct real ids never
/// collide onto one lock file.
fn task_id_lock_path(home: &Path, task_id: &str) -> PathBuf {
    home.join("task_locks")
        .join(format!("{}.lock", project_slug(task_id)))
}

/// #2760 item 2: acquire the per-task-ID router lock (the OUTER lock; the board /
/// event-log writer lock is INNER). Held across the strict-route revalidation and
/// the board append so no concurrent authority mutation on the same id can
/// interleave between them. NOTHING under this lock may perform self-IPC
/// (`api::call` / `enqueue_with_idle_hint`) — [`route_task`] only reads catalog
/// state and the checked append only reads `fleet.yaml` + takes the
/// board writer lock, so the `#1629` flock-across-self-IPC guard is respected. Any
/// cascade/cleanup/notify a caller runs MUST happen AFTER this guard drops.
pub(super) fn acquire_task_id_lock(
    home: &Path,
    task_id: &str,
) -> anyhow::Result<crate::store::FileFlockGuard> {
    crate::store::acquire_file_lock(&task_id_lock_path(home, task_id))
}

/// Resolve a globally unique task id through the authoritative catalog.
pub(super) fn route_task(
    home: &Path,
    task_id: &str,
) -> Result<(String, PathBuf, crate::task_events::TaskRecord), TaskRouteError> {
    use crate::task_events::catalog::CatalogRouteError;

    match crate::task_events::catalog::for_home(home).route(&TaskId(task_id.to_string())) {
        Ok((project, task)) => {
            let board = board_root(home, &project);
            Ok((project, board, task.current_task_record()))
        }
        Err(CatalogRouteError::NotFound) => Err(TaskRouteError::NotFound),
        Err(CatalogRouteError::Unreadable) => Err(TaskRouteError::Unreadable {
            path: home.to_path_buf(),
            cause: "task catalog is unreadable".to_string(),
        }),
        Err(CatalogRouteError::Ambiguous { boards }) => Err(TaskRouteError::Ambiguous {
            candidates: boards,
            cause: "task id is present on multiple catalog boards".to_string(),
        }),
    }
}

/// All tasks across every board, tagged with their project id — for the
/// `list project=all` / `scope=fleet` aggregate view.
pub(crate) fn list_all_boards(home: &Path) -> Vec<(String, Vec<Task>)> {
    // #2760 R1: the aggregate `list project=all` view is OUTSIDE the strict-route
    // authority scope; on an unreadable boards/ dir it degrades to the default
    // board rather than failing (a display path, not a per-id authority decision).
    enumerate_projects(home)
        .unwrap_or_else(|_| vec![DEFAULT_PROJECT.to_string()])
        .into_iter()
        .map(|project| {
            let tasks = super::list_all_at(home, &board_root(home, &project));
            (project, tasks)
        })
        .collect()
}

pub(crate) fn list_all_boards_checked(
    home: &Path,
) -> Result<Vec<(String, Vec<Task>)>, TaskRouteError> {
    enumerate_projects(home)?
        .into_iter()
        .map(|project| {
            super::list_all_at_checked(home, &board_root(home, &project))
                .map(|tasks| (project, tasks))
                .map_err(|error| TaskRouteError::Unreadable {
                    path: home.to_path_buf(),
                    cause: error.to_string(),
                })
        })
        .collect()
}

/// #2117 completeness: read EVERY project board and merge into ONE aggregate
/// `TaskBoardState` (its `tasks` map is the union across boards). The
/// multi-board view for `task action=health`. Task ids are globally unique and
/// each task lives on exactly one board, so the union never collides. The first
/// unreadable catalog board propagates its error.
pub(super) fn replay_all_boards(home: &Path) -> anyhow::Result<crate::task_events::TaskBoardState> {
    let mut merged = crate::task_events::TaskBoardState::default();
    // #2760 R1: enumeration failure fails closed (like the per-board replay below).
    for project in enumerate_projects(home).map_err(|e| anyhow::anyhow!("enumerate boards: {e}"))? {
        let state = crate::task_events::projected_state_at(&board_root(home, &project))?;
        merged.tasks.extend(state.tasks);
    }
    Ok(merged)
}

/// P0 cross-lease: strict cross-board task list for authority decisions
/// (auto-release). Unlike `list_all_boards` (display-only, degrades to
/// default on error), this function FAILS CLOSED:
///   - enumeration error → Err (cannot guarantee all boards were scanned)
///   - per-board replay error → Err (cannot guarantee task state is complete)
///   - duplicate task id across boards → Err (ambiguous identity)
///
/// Returns ALL tasks across every project board. Callers that need
/// branch-filtered subsets should filter the result.
pub(crate) fn list_all_strict(home: &Path) -> Result<Vec<Task>, TaskRouteError> {
    use crate::task_events::catalog::CatalogRouteError;

    crate::task_events::catalog::for_home(home)
        .all_tasks()
        .map(|tasks| {
            tasks
                .iter()
                .map(|task| super::record_to_task(&task.current_task_record()))
                .collect()
        })
        .map_err(|error| match error {
            CatalogRouteError::NotFound => TaskRouteError::NotFound,
            CatalogRouteError::Unreadable => TaskRouteError::Unreadable {
                path: home.to_path_buf(),
                cause: "task catalog is unreadable".to_string(),
            },
            CatalogRouteError::Ambiguous { boards } => TaskRouteError::Ambiguous {
                candidates: boards,
                cause: "duplicate task id across catalog boards".to_string(),
            },
        })
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::task_events::{append_batch_at, InstanceName, TaskEvent};

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agend-board-router-{}-{}-{tag}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn seed_task(home: &Path, project: &str, task_id: &str) {
        append_batch_at(
            &board_root(home, project),
            &InstanceName::from("test:seed"),
            vec![TaskEvent::Created {
                task_id: TaskId(task_id.to_string()),
                title: "task".into(),
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
        .unwrap();
    }

    #[test]
    fn route_task_uses_catalog_for_default_and_project_boards() {
        let home = tmp_home("route");
        seed_task(&home, DEFAULT_PROJECT, "T-default");
        seed_task(&home, "proj-a", "T-a");

        assert_eq!(route_task(&home, "T-default").unwrap().0, DEFAULT_PROJECT);
        assert_eq!(route_task(&home, "T-a").unwrap().0, "proj-a");
        assert!(matches!(
            route_task(&home, "missing"),
            Err(TaskRouteError::NotFound)
        ));
    }

    #[test]
    fn duplicate_task_id_fails_closed() {
        let home = tmp_home("duplicate");
        seed_task(&home, "proj-a", "T-dup");
        seed_task(&home, "proj-b", "T-dup");

        assert!(matches!(
            route_task(&home, "T-dup"),
            Err(TaskRouteError::Ambiguous { .. })
        ));
        assert!(matches!(
            list_all_strict(&home),
            Err(TaskRouteError::Ambiguous { .. })
        ));
    }

    #[test]
    fn replay_all_boards_aggregates_tasks_across_projects() {
        let home = tmp_home("replay-all");
        seed_task(&home, DEFAULT_PROJECT, "T-default");
        seed_task(&home, "proj-a", "T-a");

        let merged = replay_all_boards(&home).unwrap();
        let ids: std::collections::BTreeSet<&str> =
            merged.tasks.keys().map(|id| id.0.as_str()).collect();
        assert_eq!(ids, ["T-a", "T-default"].into_iter().collect());
    }
}
