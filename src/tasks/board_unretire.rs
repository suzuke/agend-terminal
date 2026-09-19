//! Guarded, audited reversal of board retirement (#3668).
//!
//! `board_sweep` moves `boards/<id>` into `boards-retired/<id>` and nothing ever
//! moved it back. This is the symmetric, equally disciplined path: dry-run
//! candidate listing, `apply=true` + non-empty `confirm_ids` + `audit_reason`,
//! fail-closed when a live board already owns the id, a rename rollback on
//! catalog-rebuild failure, and an `event-log` record.

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::task_events::board_root;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Blocker {
    LiveBoardExists,
    Unreadable(String),
}

impl Blocker {
    fn message(&self) -> String {
        match self {
            Self::LiveBoardExists => "a live board with this id already exists".to_string(),
            Self::Unreadable(cause) => format!("unreadable: {cause}"),
        }
    }
}

#[derive(Debug, Clone)]
struct RetiredBoardReport {
    project: String,
    task_count: usize,
    live_task_ids: Vec<String>,
    blockers: Vec<Blocker>,
}

impl RetiredBoardReport {
    fn candidate(&self) -> bool {
        self.blockers.is_empty()
    }

    fn json(&self) -> Value {
        serde_json::json!({
            "project": self.project,
            "tasks": self.task_count,
            "live_tasks": self.live_task_ids.len(),
            "live_task_ids": self.live_task_ids,
            "candidate": self.candidate(),
            "blockers": self.blockers.iter().map(Blocker::message).collect::<Vec<_>>(),
        })
    }
}

fn retired_root(home: &Path) -> PathBuf {
    home.join("boards-retired")
}

fn scan(home: &Path) -> Result<Vec<RetiredBoardReport>, String> {
    let retired = retired_root(home);
    let entries = match std::fs::read_dir(&retired) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", retired.display())),
    };
    let mut names = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("read retired board entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("stat retired board entry: {error}"))?;
        if file_type.is_dir() {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "non-UTF-8 retired board name".to_string())?;
            names.insert(name);
        }
    }

    Ok(names
        .into_iter()
        .map(|project| {
            let mut blockers = Vec::new();
            if board_root(home, &project).exists() {
                blockers.push(Blocker::LiveBoardExists);
            }
            let (task_count, mut live_task_ids) =
                match crate::task_events::disk_state_at(&retired.join(&project)) {
                    Ok(state) => {
                        let live: Vec<String> = state
                            .tasks
                            .values()
                            .filter(|record| !record.status.is_terminal())
                            .map(|record| record.id.0.clone())
                            .collect();
                        (state.tasks.len(), live)
                    }
                    Err(error) => {
                        blockers.push(Blocker::Unreadable(error.to_string()));
                        (0, Vec::new())
                    }
                };
            live_task_ids.sort();
            RetiredBoardReport {
                project,
                task_count,
                live_task_ids,
                blockers,
            }
        })
        .collect())
}

#[derive(Debug)]
struct UnretireOutcome {
    project: String,
    moved_to: Option<PathBuf>,
    error: Option<String>,
}

fn unretire(home: &Path, confirmed: &[String], audit_reason: &str) -> Vec<UnretireOutcome> {
    let mut outcomes = Vec::new();
    for project in confirmed {
        let _board_set_lock = match crate::task_events::acquire_board_set_lock(home) {
            Ok(lock) => lock,
            Err(error) => {
                outcomes.push(UnretireOutcome {
                    project: project.clone(),
                    moved_to: None,
                    error: Some(format!("lock board set: {error}")),
                });
                continue;
            }
        };
        let source = retired_root(home).join(project);
        if !source.is_dir() {
            outcomes.push(UnretireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some("no such retired board".to_string()),
            });
            continue;
        }
        // Fail-closed reverse guard: never clobber a live board that now owns
        // this id.
        let destination = board_root(home, project);
        if destination.exists() {
            outcomes.push(UnretireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some(format!(
                    "live board already exists: {}",
                    destination.display()
                )),
            });
            continue;
        }
        // Scan (which strictly folds the retired board from disk) BEFORE taking
        // the board lock: the fold may itself take the writer lock to repair a
        // torn tail, and a second flock on the same path in one process would
        // deadlock. Retired boards reject appends, so no writer can slip in.
        let reports = match scan(home) {
            Ok(reports) => reports,
            Err(error) => {
                outcomes.push(UnretireOutcome {
                    project: project.clone(),
                    moved_to: None,
                    error: Some(error),
                });
                continue;
            }
        };
        let Some(report) = reports.iter().find(|report| &report.project == project) else {
            outcomes.push(UnretireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some("no such retired board".to_string()),
            });
            continue;
        };
        if !report.candidate() {
            outcomes.push(UnretireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some(format!(
                    "not a candidate: {}",
                    report
                        .blockers
                        .iter()
                        .map(Blocker::message)
                        .collect::<Vec<_>>()
                        .join("; ")
                )),
            });
            continue;
        }

        let _board_lock = match crate::store::acquire_file_lock(
            &crate::task_events::board_event_lock_path(&source),
        ) {
            Ok(lock) => lock,
            Err(error) => {
                outcomes.push(UnretireOutcome {
                    project: project.clone(),
                    moved_to: None,
                    error: Some(format!("lock retired board '{project}': {error}")),
                });
                continue;
            }
        };

        let Some(parent) = destination.parent() else {
            outcomes.push(UnretireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some(format!("no parent for {}", destination.display())),
            });
            continue;
        };
        if let Err(error) = std::fs::create_dir_all(parent) {
            outcomes.push(UnretireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some(format!("create {}: {error}", parent.display())),
            });
            continue;
        }
        drop(_board_lock);
        if let Err(error) = std::fs::rename(&source, &destination) {
            outcomes.push(UnretireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some(format!("rename failed: {error}")),
            });
            continue;
        }
        if let Err(error) = crate::task_events::catalog::rebuild_after_board_change_locked(home) {
            let rollback = std::fs::rename(&destination, &source);
            let _ = crate::task_events::catalog::rebuild_after_board_change_locked(home);
            outcomes.push(UnretireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some(format!(
                    "catalog rebuild failed: {error:?}; rollback {}",
                    if rollback.is_ok() {
                        "succeeded"
                    } else {
                        "failed"
                    }
                )),
            });
            continue;
        }

        crate::event_log::log(
            home,
            "board_unretired",
            "task-board",
            &format!(
                "unretired board '{project}' ({} tasks, {} reactivated) from {}: {audit_reason}",
                report.task_count,
                report.live_task_ids.len(),
                destination.display()
            ),
        );
        outcomes.push(UnretireOutcome {
            project: project.clone(),
            moved_to: Some(destination),
            error: None,
        });
    }
    outcomes
}

pub(super) fn handle(home: &Path, args: &Value) -> Value {
    let reports = match scan(home) {
        Ok(reports) => reports,
        Err(error) => return serde_json::json!({"error": error}),
    };
    if !args["apply"].as_bool().unwrap_or(false) {
        let candidates: Vec<_> = reports
            .iter()
            .filter(|report| report.candidate())
            .map(|report| report.project.clone())
            .collect();
        return serde_json::json!({
            "dry_run": true,
            "boards": reports.iter().map(RetiredBoardReport::json).collect::<Vec<_>>(),
            "candidate_ids": candidates,
            "total_candidates": reports.iter().filter(|report| report.candidate()).count(),
            "unretire_target": "boards/ (move, not copy)",
        });
    }

    let confirmed: Vec<String> = args["confirm_ids"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if confirmed.is_empty() {
        return serde_json::json!({"error": "apply=true requires non-empty confirm_ids"});
    }
    let reason = args["audit_reason"].as_str().unwrap_or("").trim();
    if reason.is_empty() {
        return serde_json::json!({"error": "apply=true requires non-empty audit_reason"});
    }

    let outcomes = unretire(home, &confirmed, reason);
    serde_json::json!({
        "applied": true,
        "audit_reason": reason,
        "unretired_count": outcomes.iter().filter(|outcome| outcome.error.is_none()).count(),
        "results": outcomes.iter().map(|outcome| serde_json::json!({
            "project": outcome.project,
            "moved_to": outcome.moved_to.as_ref().map(|path| path.display().to_string()),
            "error": outcome.error,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::task_events::{board_root, InstanceName, TaskEvent, TaskId};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn home(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agend-board-unretire-{}-{tag}-{}",
            std::process::id(),
            HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(home.join("boards")).unwrap();
        home
    }

    fn created(id: &str) -> TaskEvent {
        TaskEvent::Created {
            task_id: TaskId(id.into()),
            title: "test".into(),
            description: String::new(),
            priority: "normal".into(),
            owner: None,
            due_at: None,
            depends_on: Vec::new(),
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: Vec::new(),
            parent_id: None,
            governing_decision_id: None,
            review_class: None,
        }
    }

    fn task(board: &Path, id: &str, terminal: bool) {
        let instance = InstanceName("seed".into());
        crate::task_events::append_at(board, &instance, created(id)).unwrap();
        if terminal {
            crate::task_events::append_at(
                board,
                &instance,
                TaskEvent::Cancelled {
                    task_id: TaskId(id.into()),
                    by: instance.clone(),
                    reason: "done".into(),
                },
            )
            .unwrap();
        }
    }

    /// Seed a LIVE board with one task, then move it into `boards-retired/`
    /// directly (the state a prior `board_sweep` leaves behind).
    fn seed_retired(home: &Path, board: &str, id: &str, terminal: bool) {
        let live = board_root(home, board);
        std::fs::create_dir_all(&live).unwrap();
        task(&live, id, terminal);
        let retired = home.join("boards-retired");
        std::fs::create_dir_all(&retired).unwrap();
        std::fs::rename(&live, retired.join(board)).unwrap();
    }

    fn copy_dir_all(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let to = dst.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir_all(&entry.path(), &to);
            } else {
                std::fs::copy(entry.path(), &to).unwrap();
            }
        }
    }

    fn apply(home: &Path, ids: &[&str]) -> Value {
        handle(
            home,
            &serde_json::json!({
                "apply": true,
                "confirm_ids": ids,
                "audit_reason": "test unretire",
            }),
        )
    }

    #[test]
    fn dry_run_lists_candidates_without_changing_filesystem() {
        let home = home("dry-run");
        seed_retired(&home, "stale", "t-stale", true);

        let result = handle(&home, &serde_json::json!({}));
        assert_eq!(result["dry_run"], true);
        assert!(result["candidate_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "stale"));
        // No filesystem mutation during dry-run.
        assert!(home.join("boards-retired/stale").exists());
        assert!(!board_root(&home, "stale").exists());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn apply_moves_retired_board_back_and_board_is_writable() {
        let home = home("apply");
        seed_retired(&home, "stale", "t-stale", true);

        let result = apply(&home, &["stale"]);
        assert_eq!(result["unretired_count"], 1, "{result}");
        assert!(!home.join("boards-retired/stale").exists());
        let live = board_root(&home, "stale");
        assert!(live.exists());
        // History survived the round trip and the board is writable again.
        assert!(crate::tasks::load_routed(&home, "t-stale").is_ok());
        let instance = InstanceName("late".into());
        crate::task_events::append_at(&live, &instance, created("t-late")).unwrap();
        assert!(crate::tasks::load_routed(&home, "t-late").is_ok());
        // Audited like retirement.
        let audit = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap();
        assert!(audit.contains("board_unretired"), "{audit}");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn apply_refuses_when_live_board_exists() {
        let home = home("collision");
        seed_retired(&home, "dup", "t-retired", true);
        // A live board now owns the same id. (`append_at` would refuse while the
        // retired twin exists, so recreate it by copying the retired directory.)
        copy_dir_all(&home.join("boards-retired/dup"), &board_root(&home, "dup"));

        let result = apply(&home, &["dup"]);
        assert_eq!(result["unretired_count"], 0, "{result}");
        assert!(result["results"][0]["error"].is_string(), "{result}");
        // Fail-closed: nothing was overwritten or moved.
        assert!(home.join("boards-retired/dup").exists());
        assert!(board_root(&home, "dup").join("task_events.jsonl").exists());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn dry_run_discloses_tasks_reactivated_by_unretire() {
        let home = home("disclosure");
        seed_retired(&home, "hot", "t-hot", false);

        let result = handle(&home, &serde_json::json!({}));
        let board = result["boards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|board| board["project"] == "hot")
            .unwrap();
        assert_eq!(board["candidate"], true, "{board}");
        assert_eq!(board["live_tasks"], 1, "{board}");
        assert!(board["live_task_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == "t-hot"));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn failed_move_leaves_retired_board_intact() {
        let home = home("move-failure");
        seed_retired(&home, "stuck", "t-stuck", true);
        // Make `boards/` un-creatable so the move cannot land.
        std::fs::remove_dir_all(home.join("boards")).unwrap();
        std::fs::write(home.join("boards"), b"not a directory").unwrap();

        let result = apply(&home, &["stuck"]);
        assert_eq!(result["unretired_count"], 0, "{result}");
        assert!(result["results"][0]["error"].is_string(), "{result}");
        assert!(home.join("boards-retired/stuck").exists());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn apply_requires_confirmation_and_reason() {
        let home = home("contract");
        assert!(handle(&home, &serde_json::json!({"apply": true}))["error"].is_string());
        assert!(handle(
            &home,
            &serde_json::json!({"apply": true, "confirm_ids": ["stale"]})
        )["error"]
            .is_string());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn retry_after_unretire_is_safe_and_idempotent() {
        let home = home("retry");
        seed_retired(&home, "once", "t-once", true);
        assert_eq!(apply(&home, &["once"])["unretired_count"], 1);

        let second = apply(&home, &["once"]);
        assert_eq!(second["unretired_count"], 0, "{second}");
        assert!(second["results"][0]["error"].is_string(), "{second}");
        // The board survives the refused retry.
        assert!(board_root(&home, "once").exists());
        assert!(crate::tasks::load_routed(&home, "t-once").is_ok());
        std::fs::remove_dir_all(home).ok();
    }
}
