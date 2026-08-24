//! Bounded daemon RPC used by the permanent APP thin client.

use serde_json::Value;
use std::path::Path;

pub(super) enum TaskRequest {
    List,
    Mutate(Value),
}

pub(super) type TaskOutcome = Result<Vec<crate::tasks::Task>, String>;

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
    let response = crate::api::call_at(
        run_dir,
        &serde_json::json!({
            "method": crate::api::method::MCP_TOOL,
            "params": {"tool": "task", "arguments": arguments, "instance": ""}
        }),
    )
    .map_err(|error| error.to_string())?;
    if response["ok"].as_bool() == Some(true) {
        Ok(response["result"].clone())
    } else {
        Err(response["error"]
            .as_str()
            .unwrap_or("daemon rejected task request")
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn app_task_rpc_uses_the_bounded_cross_process_call() {
        let source = include_str!("rpc.rs");
        assert!(source.contains("crate::api::call_at("));
    }

    #[test]
    fn app_production_task_paths_do_not_read_or_write_task_files() {
        for (path, source) in [
            ("dispatch.rs", include_str!("dispatch.rs")),
            ("overlay.rs", include_str!("overlay.rs")),
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
}
