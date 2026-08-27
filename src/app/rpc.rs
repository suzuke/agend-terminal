//! Bounded daemon RPC used by the permanent APP thin client.

use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub(super) type AgentStateSnapshot = HashMap<String, Option<crate::state::AgentState>>;

pub(super) enum AgentStateRequest {
    Refresh,
}

pub(super) enum TaskRequest {
    List,
    Mutate(Value),
}

pub(super) type TaskOutcome = Result<Vec<crate::tasks::Task>, String>;

pub(super) type AgentStateOutcome = Result<AgentStateSnapshot, String>;

pub(super) fn spawn_task_worker(
    run_dir: &Path,
) -> (
    crossbeam_channel::Sender<TaskRequest>,
    crossbeam_channel::Receiver<TaskOutcome>,
    std::thread::JoinHandle<()>,
) {
    let (request_tx, request_rx) = crossbeam_channel::bounded(1);
    let (outcome_tx, outcome_rx) = crossbeam_channel::unbounded();
    let run_dir = run_dir.to_path_buf();
    // fire-and-forget: false; run_app joins the returned handle after dropping the sender.
    let worker = std::thread::Builder::new()
        .name("app-task-rpc".into())
        .spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let outcome = execute(&run_dir, request);
                if outcome_tx.send(outcome).is_err() {
                    break;
                }
            }
        })
        .expect("spawn app task RPC worker");
    (request_tx, outcome_rx, worker)
}

pub(super) fn spawn_agent_state_worker(
    run_dir: &Path,
) -> (
    crossbeam_channel::Sender<AgentStateRequest>,
    crossbeam_channel::Receiver<AgentStateOutcome>,
    std::thread::JoinHandle<()>,
) {
    let (request_tx, request_rx) = crossbeam_channel::bounded(1);
    let (outcome_tx, outcome_rx) = crossbeam_channel::bounded(1);
    let run_dir = run_dir.to_path_buf();
    // fire-and-forget: false; run_app joins the returned handle after dropping the sender.
    let worker = std::thread::Builder::new()
        .name("app-agent-state-rpc".into())
        .spawn(move || {
            while let Ok(AgentStateRequest::Refresh) = request_rx.recv() {
                if outcome_tx.send(list_instances(&run_dir)).is_err() {
                    break;
                }
            }
        })
        .expect("spawn app agent-state RPC worker");
    (request_tx, outcome_rx, worker)
}

fn execute(run_dir: &Path, request: TaskRequest) -> TaskOutcome {
    if let TaskRequest::Mutate(arguments) = request {
        call_task(run_dir, arguments)
            .map_err(|error| format!("task write failed or timed out; outcome unknown: {error}"))?;
    }
    let result = call_task(
        run_dir,
        serde_json::json!({"action": "list", "include_history": true, "verbose": true}),
    )?;
    serde_json::from_value(result["tasks"].clone())
        .map_err(|error| format!("invalid task list from daemon: {error}"))
}

fn call_task(run_dir: &Path, arguments: Value) -> Result<Value, String> {
    call_tool(
        run_dir,
        "task",
        arguments,
        std::time::Duration::from_secs(10),
    )
}

pub(super) fn list_instances(run_dir: &Path) -> Result<AgentStateSnapshot, String> {
    let result = call_tool(
        run_dir,
        "list_instances",
        serde_json::json!({}),
        std::time::Duration::from_secs(5),
    )?;
    let instances = result["instances"]
        .as_array()
        .ok_or_else(|| "invalid list_instances response: missing instances".to_string())?;
    let mut states = HashMap::new();
    for instance in instances {
        let Some(name) = instance["name"].as_str() else {
            continue;
        };
        states.insert(
            name.to_string(),
            instance["agent_state"].as_str().and_then(parse_agent_state),
        );
    }
    Ok(states)
}

fn call_tool(
    run_dir: &Path,
    tool: &str,
    arguments: Value,
    timeout: std::time::Duration,
) -> Result<Value, String> {
    let response = crate::api::call_at(
        run_dir,
        &serde_json::json!({
            "method": crate::api::method::MCP_TOOL,
            "params": {"tool": tool, "arguments": arguments, "instance": ""}
        }),
        timeout,
    )
    .map_err(|error| error.to_string())?;
    if response["ok"].as_bool() == Some(true) {
        Ok(response["result"].clone())
    } else {
        Err(response["error"]
            .as_str()
            .unwrap_or("daemon rejected MCP tool request")
            .to_string())
    }
}

fn parse_agent_state(raw: &str) -> Option<crate::state::AgentState> {
    use crate::state::AgentState;
    Some(match raw {
        "starting" => AgentState::Starting,
        "hang" => AgentState::Hang,
        "awaiting_operator" => AgentState::AwaitingOperator,
        "idle" => AgentState::Idle,
        "active" => AgentState::Active,
        "interactive_prompt" => AgentState::InteractivePrompt,
        "permission" => AgentState::PermissionPrompt,
        "git_conflict" => AgentState::GitConflict,
        "context_full" => AgentState::ContextFull,
        "rate_limit" => AgentState::RateLimit,
        "server_rate_limit" => AgentState::ServerRateLimit,
        "usage_limit" => AgentState::UsageLimit,
        "auth_error" => AgentState::AuthError,
        "api_error" => AgentState::ApiError,
        "model_unsupported" => AgentState::ModelUnsupported,
        "crashed" => AgentState::Crashed,
        "restarting" => AgentState::Restarting,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[test]
    fn agent_state_worker_resolves_each_request_against_successor_without_sleep() {
        let old = std::path::PathBuf::from("/run/old-generation");
        let new = std::path::PathBuf::from("/run/new-generation");
        let resolved = Arc::new(Mutex::new(VecDeque::from([old.clone(), new.clone()])));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let resolver = {
            let resolved = Arc::clone(&resolved);
            Arc::new(move |_home: &std::path::Path| {
                resolved.lock().unwrap().pop_front()
            })
        };
        let caller = {
            let calls = Arc::clone(&calls);
            Arc::new(move |run_dir: &std::path::Path,
                          _tool: &str,
                          _arguments: Value,
                          _timeout: std::time::Duration| {
                calls.lock().unwrap().push(run_dir.to_path_buf());
                let name = run_dir.file_name().unwrap().to_string_lossy().to_string();
                Ok(serde_json::json!({
                    "instances": [{"name": name, "agent_state": "idle"}]
                }))
            })
        };
        let (_tx, rx, worker) = super::spawn_agent_state_worker_with_seams(
            std::path::Path::new("/home"),
            resolver,
            caller,
        );
        // The seam is request-driven: no wall-clock waits or daemon required.
        _tx.send(AgentStateRequest::Refresh).unwrap();
        _tx.send(AgentStateRequest::Refresh).unwrap();
        let first = rx.recv().unwrap().unwrap();
        let second = rx.recv().unwrap().unwrap();
        assert!(first.snapshot.contains_key("old-generation"));
        assert!(second.snapshot.contains_key("new-generation"));
        assert_eq!(&*calls.lock().unwrap(), &[old, new]);
        drop(_tx);
        worker.join().unwrap();
    }

    #[test]
    fn mutate_applied_refresh_failed_is_distinct_and_does_not_replay_write() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let caller = {
            let calls = Arc::clone(&calls);
            move |_run_dir: &std::path::Path,
                  tool: &str,
                  _arguments: Value,
                  _timeout: std::time::Duration| {
                calls.lock().unwrap().push(tool.to_string());
                if calls.lock().unwrap().len() == 1 {
                    Ok(serde_json::json!({"ok": true}))
                } else {
                    Err("successor list unavailable".to_string())
                }
            }
        };
        let outcome = super::execute_with_seams(
            std::path::Path::new("/home"),
            TaskRequest::Mutate(serde_json::json!({"action": "update", "id": "t-1"})),
            |_home| Some(std::path::PathBuf::from("/run/current")),
            caller,
        );
        assert!(matches!(
            outcome,
            TaskOutcome::MutationAppliedRefreshFailed { .. }
        ));
        assert_eq!(&*calls.lock().unwrap(), &["task", "task"]);
    }

    #[test]
    fn app_task_rpc_uses_the_bounded_cross_process_call() {
        let source = include_str!("rpc.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(production.contains("crate::api::call_at("));
        assert!(production.contains("std::time::Duration::from_secs(10)"));
    }

    #[test]
    fn app_production_task_paths_do_not_read_or_write_task_files() {
        for (path, source) in [
            ("dispatch.rs", include_str!("dispatch.rs")),
            ("overlay.rs", include_str!("overlay.rs")),
            ("panels.rs", include_str!("../render/panels.rs")),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            assert!(
                !production.contains("crate::tasks::handle("),
                "{path} bypasses daemon task RPC"
            );
            assert!(
                !production.contains("crate::tasks::list_all("),
                "{path} bypasses daemon task RPC"
            );
        }
    }

    #[test]
    fn list_instances_maps_known_states_and_preserves_unknown() {
        assert_eq!(
            super::parse_agent_state("active"),
            Some(crate::state::AgentState::Active)
        );
        assert_eq!(super::parse_agent_state("future_state"), None);
    }
}
