//! Guarded, recoverable retirement for stale project boards (#3337).

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::task_events::{board_root, project_slug};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Blocker {
    ReferencedByTeam(String),
    HasLiveTasks(usize),
    Unreadable(String),
}

impl Blocker {
    fn message(&self) -> String {
        match self {
            Self::ReferencedByTeam(team) => format!("referenced by team '{team}'"),
            Self::HasLiveTasks(count) => format!("has {count} non-terminal task(s)"),
            Self::Unreadable(cause) => format!("unreadable: {cause}"),
        }
    }
}

#[derive(Debug, Clone)]
struct BoardReport {
    project: String,
    task_count: usize,
    live_task_count: usize,
    blockers: Vec<Blocker>,
}

impl BoardReport {
    fn candidate(&self) -> bool {
        self.blockers.is_empty()
    }

    fn json(&self) -> Value {
        serde_json::json!({
            "project": self.project,
            "tasks": self.task_count,
            "live_tasks": self.live_task_count,
            "candidate": self.candidate(),
            "blockers": self.blockers.iter().map(Blocker::message).collect::<Vec<_>>(),
        })
    }
}

fn retired_root(home: &Path) -> PathBuf {
    home.join("boards-retired")
}

fn team_references(home: &Path) -> Result<Vec<(String, String)>, String> {
    let fleet = crate::teams::try_load_fleet(home).map_err(|error| error.to_string())?;
    let mut references = Vec::new();
    for (name, team) in fleet.teams {
        if let Some(project) = team.project_id {
            references.push((project_slug(&project), name));
        } else if let Some(repo) = team.source_repo {
            references.push((
                super::board_router::project_id_from_source_repo(&repo),
                name,
            ));
        }
    }
    Ok(references)
}

fn scan(home: &Path) -> Result<Vec<BoardReport>, String> {
    let boards = home.join("boards");
    let entries = match std::fs::read_dir(&boards) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", boards.display())),
    };
    let references = team_references(home)?;
    let mut names = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("read board entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("stat board entry: {error}"))?;
        if file_type.is_dir() {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "non-UTF-8 board name".to_string())?;
            names.insert(name);
        }
    }

    Ok(names
        .into_iter()
        .map(|project| {
            let mut blockers = Vec::new();
            let blocking_teams: BTreeSet<_> = references
                .iter()
                .filter(|(board, _)| board == &project)
                .map(|(_, team)| team.as_str())
                .collect();
            blockers.extend(
                blocking_teams
                    .into_iter()
                    .map(|team| Blocker::ReferencedByTeam(team.to_string())),
            );

            let state = crate::task_events::projected_state_at(&board_root(home, &project));
            let (task_count, live_task_count) = match state {
                Ok(state) => {
                    let live = state
                        .tasks
                        .values()
                        .filter(|record| !record.status.is_terminal())
                        .count();
                    (state.tasks.len(), live)
                }
                Err(error) => {
                    blockers.push(Blocker::Unreadable(error.to_string()));
                    (0, 0)
                }
            };
            if live_task_count > 0 {
                blockers.push(Blocker::HasLiveTasks(live_task_count));
            }
            BoardReport {
                project,
                task_count,
                live_task_count,
                blockers,
            }
        })
        .collect())
}

#[derive(Debug)]
struct RetireOutcome {
    project: String,
    moved_to: Option<PathBuf>,
    error: Option<String>,
}

fn retire(home: &Path, confirmed: &[String], audit_reason: &str) -> Vec<RetireOutcome> {
    let mut outcomes = Vec::new();
    for project in confirmed {
        let _board_set_lock = match crate::task_events::acquire_board_set_lock(home) {
            Ok(lock) => lock,
            Err(error) => {
                outcomes.push(RetireOutcome {
                    project: project.clone(),
                    moved_to: None,
                    error: Some(format!("lock board set: {error}")),
                });
                continue;
            }
        };
        let source = board_root(home, project);
        if !source.is_dir() {
            outcomes.push(RetireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some("no such board".to_string()),
            });
            continue;
        }
        let _board_lock = match crate::store::acquire_file_lock(
            &crate::task_events::board_event_lock_path(&source),
        ) {
            Ok(lock) => lock,
            Err(error) => {
                outcomes.push(RetireOutcome {
                    project: project.clone(),
                    moved_to: None,
                    error: Some(format!("lock board '{project}': {error}")),
                });
                continue;
            }
        };
        let reports = match scan(home) {
            Ok(reports) => reports,
            Err(error) => {
                outcomes.push(RetireOutcome {
                    project: project.clone(),
                    moved_to: None,
                    error: Some(error),
                });
                continue;
            }
        };
        let Some(report) = reports.iter().find(|report| &report.project == project) else {
            outcomes.push(RetireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some("no such board".to_string()),
            });
            continue;
        };
        if !report.candidate() {
            outcomes.push(RetireOutcome {
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

        let destination_root = retired_root(home);
        if let Err(error) = std::fs::create_dir_all(&destination_root) {
            outcomes.push(RetireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some(format!("create {}: {error}", destination_root.display())),
            });
            continue;
        }
        let destination = destination_root.join(project);
        if destination.exists() {
            outcomes.push(RetireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some(format!(
                    "retired destination already exists: {}",
                    destination.display()
                )),
            });
            continue;
        }
        if let Err(error) = std::fs::rename(&source, &destination) {
            outcomes.push(RetireOutcome {
                project: project.clone(),
                moved_to: None,
                error: Some(format!("rename failed: {error}")),
            });
            continue;
        }
        drop(_board_lock);

        if let Err(error) = crate::task_events::catalog::rebuild_after_board_change_locked(home) {
            let rollback = std::fs::rename(&destination, &source);
            let _ = crate::task_events::catalog::rebuild_after_board_change_locked(home);
            outcomes.push(RetireOutcome {
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
            "board_retired",
            "task-board",
            &format!(
                "retired board '{project}' ({} tasks) to {}: {audit_reason}",
                report.task_count,
                destination.display()
            ),
        );
        outcomes.push(RetireOutcome {
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
            "boards": reports.iter().map(BoardReport::json).collect::<Vec<_>>(),
            "candidate_ids": candidates,
            "total_candidates": reports.iter().filter(|report| report.candidate()).count(),
            "retire_target": "boards-retired/ (move, not delete)",
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

    let outcomes = retire(home, &confirmed, reason);
    serde_json::json!({
        "applied": true,
        "audit_reason": reason,
        "retired_count": outcomes.iter().filter(|outcome| outcome.error.is_none()).count(),
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
    use crate::task_events::{InstanceName, TaskEvent, TaskId};
    use std::sync::atomic::{AtomicU64, Ordering};

    static HOME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn home(tag: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "agend-board-sweep-{}-{tag}-{}",
            std::process::id(),
            HOME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(home.join("boards")).unwrap();
        home
    }

    fn task(home: &Path, board: &str, id: &str, terminal: bool) {
        let board = board_root(home, board);
        std::fs::create_dir_all(&board).unwrap();
        let instance = InstanceName("seed".into());
        crate::task_events::append_at(
            &board,
            &instance,
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
            },
        )
        .unwrap();
        if terminal {
            crate::task_events::append_at(
                &board,
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

    #[test]
    fn dry_run_offers_only_unreferenced_terminal_boards() {
        let home = home("dry-run");
        task(&home, "dead", "t-dead", true);
        task(&home, "live", "t-live", false);
        crate::teams::create(
            &home,
            &serde_json::json!({
                "name": "owners",
                "members": ["lead"],
                "orchestrator": "lead",
                "project_id": "dead",
            }),
        );

        let reports = scan(&home).unwrap();
        assert!(!reports
            .iter()
            .find(|r| r.project == "dead")
            .unwrap()
            .candidate());
        assert!(!reports
            .iter()
            .find(|r| r.project == "live")
            .unwrap()
            .candidate());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn project_id_reference_takes_priority_over_source_repo_fallback() {
        let home = home("project-id-priority");
        task(&home, "owner_repo", "t-current", true);
        task(&home, "checkout_repo", "t-obsolete", true);
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "teams:\n  owners:\n    members: [lead]\n    project_id: owner_repo\n    source_repo: /tmp/checkout/repo\n",
        )
        .unwrap();

        let reports = scan(&home).unwrap();
        assert!(!reports
            .iter()
            .find(|report| report.project == "owner_repo")
            .unwrap()
            .candidate());
        assert!(reports
            .iter()
            .find(|report| report.project == "checkout_repo")
            .unwrap()
            .candidate());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn confirmed_terminal_board_is_moved_and_catalog_is_rebuilt() {
        let home = home("retire");
        task(&home, "dead", "t-dead", true);
        let result = retire(&home, &["dead".to_string()], "test");
        assert!(result[0].error.is_none(), "{:?}", result[0].error);
        assert!(!board_root(&home, "dead").exists());
        assert!(home.join("boards-retired/dead").exists());
        assert!(crate::tasks::load_routed(&home, "t-dead").is_err());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn retired_board_rejects_late_task_writes() {
        let home = home("late-write");
        task(&home, "dead", "t-dead", true);
        let result = retire(&home, &["dead".to_string()], "test");
        assert!(result[0].error.is_none(), "{:?}", result[0].error);

        let late = crate::task_events::append_at(
            &board_root(&home, "dead"),
            &InstanceName("late".into()),
            TaskEvent::Created {
                task_id: TaskId("t-late".into()),
                title: "late".into(),
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
            },
        );
        assert!(late.is_err(), "retired board must reject late writes");
        assert!(!board_root(&home, "dead").exists());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn apply_requires_confirmation_and_reason() {
        let home = home("contract");
        assert!(handle(&home, &serde_json::json!({"apply": true}))["error"].is_string());
        assert!(handle(
            &home,
            &serde_json::json!({"apply": true, "confirm_ids": ["dead"]})
        )["error"]
            .is_string());
        std::fs::remove_dir_all(home).ok();
    }
}
