//! Bounded daemon RPC used by the permanent APP thin client.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};

pub(super) type AgentStateSnapshot = HashMap<String, Option<crate::state::AgentState>>;

pub(super) enum AgentStateRequest {
    Refresh,
}

pub(super) enum TaskRequest {
    List,
    Mutate(Value),
}

#[derive(Debug)]
pub(super) enum TaskOutcome {
    Snapshot(Vec<crate::tasks::Task>),
    MutationApplied { snapshot: Vec<crate::tasks::Task> },
    MutationAppliedRefreshFailed { error: String },
    MutationFailedUnknown { error: String },
    Failed { error: String },
}

#[derive(Debug)]
pub(super) struct AgentStateSnapshotResult {
    pub(super) snapshot: AgentStateSnapshot,
    pub(super) names: HashSet<String>,
    pub(super) instance_refs: HashMap<String, crate::types::InstanceRef>,
    pub(super) mode: crate::runtime::AgentListMode,
}

#[derive(Debug)]
pub(super) struct AgentStateError {
    pub(super) error: String,
    pub(super) mode: crate::runtime::AgentListMode,
}

pub(super) type AgentStateOutcome = Result<AgentStateSnapshotResult, AgentStateError>;

#[derive(Debug)]
pub(super) enum EventStreamOutcome {
    Event(crate::daemon::event_hub::DaemonEvent),
    Disconnected(String),
}

/// Subscribe on a dedicated authenticated API connection. The worker owns
/// only the stream; AppState remains the sole owner of UI mutation.
pub(super) fn spawn_event_worker(
    home: &Path,
) -> (
    crossbeam_channel::Sender<()>,
    crossbeam_channel::Receiver<EventStreamOutcome>,
    std::thread::JoinHandle<()>,
) {
    let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
    let (outcome_tx, outcome_rx) = crossbeam_channel::bounded(64);
    let home = home.to_path_buf();
    // fire-and-forget: false; run_app joins this worker during teardown.
    let worker = std::thread::Builder::new()
        .name("app-event-stream".into())
        .spawn(move || {
            let run_dir = match resolve_active_run_dir(&home) {
                Some(run_dir) => run_dir,
                None => {
                    let _ = outcome_tx.try_send(EventStreamOutcome::Disconnected(
                        "no active daemon".to_string(),
                    ));
                    return;
                }
            };
            let (mut reader, stream_source) = match open_event_stream(&run_dir) {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = outcome_tx.try_send(EventStreamOutcome::Disconnected(error));
                    return;
                }
            };
            loop {
                if stop_rx.try_recv().is_ok() {
                    return;
                }
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = outcome_tx.try_send(EventStreamOutcome::Disconnected(
                            "event stream closed".to_string(),
                        ));
                        return;
                    }
                    Ok(_) => {
                        match serde_json::from_str::<crate::daemon::event_hub::DaemonEvent>(&line) {
                            Ok(event) if event.source == stream_source => {
                                if outcome_tx
                                    .try_send(EventStreamOutcome::Event(event))
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Ok(_) => {
                                let _ = outcome_tx.try_send(EventStreamOutcome::Disconnected(
                                    "event stream source changed".to_string(),
                                ));
                                return;
                            }
                            Err(error) => {
                                let _ = outcome_tx.try_send(EventStreamOutcome::Disconnected(
                                    format!("invalid event stream payload: {error}"),
                                ));
                                return;
                            }
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) => {}
                    Err(error) => {
                        let _ = outcome_tx
                            .try_send(EventStreamOutcome::Disconnected(error.to_string()));
                        return;
                    }
                }
            }
        })
        .expect("spawn app event stream worker");
    (stop_tx, outcome_rx, worker)
}

fn open_event_stream(run_dir: &Path) -> Result<(BufReader<TcpStream>, String), String> {
    let stream = crate::ipc::connect_run_dir_api(run_dir).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(250)))
        .map_err(|error| error.to_string())?;
    let operator_token =
        crate::auth_cookie::read_operator_token(run_dir).map_err(|error| error.to_string())?;
    let mut writer = stream.try_clone().map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, &operator_token)
        .map_err(|error| error.to_string())?;
    writeln!(
        writer,
        "{}",
        serde_json::json!({
            "method": crate::api::method::SUBSCRIBE_EVENTS,
            "params": {},
        })
    )
    .map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())?;
    let mut ack = String::new();
    reader
        .read_line(&mut ack)
        .map_err(|error| error.to_string())?;
    let ack: Value = serde_json::from_str(&ack).map_err(|error| error.to_string())?;
    if ack["ok"].as_bool() != Some(true) {
        return Err(ack["error"]
            .as_str()
            .unwrap_or("event stream rejected")
            .to_string());
    }
    let source = ack["event_stream"]["source"]
        .as_str()
        .filter(|source| !source.is_empty() && *source != "unknown")
        .ok_or_else(|| "event stream has unverifiable source".to_string())?;
    let expected_source = crate::daemon::event_hub::source_id(run_dir);
    if expected_source == "unknown" || source != expected_source {
        return Err("event stream source does not match active daemon".to_string());
    }
    Ok((reader, source.to_string()))
}

/// Create a managed instance through the daemon's lifecycle API.
///
/// The app is a thin client: it may request a new instance, but it must never
/// fork a second app-local child for a fleet identity. The daemon persists the
/// fleet entry, owns the process, and publishes the bridge endpoint before this
/// returns.
pub(super) fn create_instance(
    home: &Path,
    name: &str,
    backend: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<String, String> {
    create_instance_with(
        home,
        name,
        backend,
        args,
        env,
        resolve_active_run_dir,
        call_tool_at,
    )
}

/// Restart a daemon-owned instance through the authenticated daemon API.
pub(super) fn restart_instance(home: &Path, name: &str) -> Result<(), String> {
    restart_instance_with(home, name, resolve_active_run_dir, call_tool_at)
}

fn restart_instance_with<R, C>(
    home: &Path,
    name: &str,
    resolver: R,
    caller: C,
) -> Result<(), String>
where
    R: Fn(&Path) -> Option<PathBuf>,
    C: Fn(&Path, &str, Value, std::time::Duration) -> Result<Value, String>,
{
    let Some(run_dir) = resolver(home) else {
        return Err("no active daemon (run dir not found)".to_string());
    };
    let result = caller(
        &run_dir,
        "restart_instance",
        serde_json::json!({
            "instance": name,
            "mode": "resume",
            "reason": "manual TUI :restart",
        }),
        std::time::Duration::from_secs(60),
    )?;
    if let Some(error) = result.get("error").and_then(Value::as_str) {
        return Err(error.to_string());
    }
    if result.get("spawned").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(format!("daemon restart_instance did not spawn '{name}'"))
    }
}

fn create_instance_with<R, C>(
    home: &Path,
    name: &str,
    backend: &str,
    args: &[String],
    env: &HashMap<String, String>,
    resolver: R,
    caller: C,
) -> Result<String, String>
where
    R: Fn(&Path) -> Option<PathBuf>,
    C: Fn(&Path, &str, Value, std::time::Duration) -> Result<Value, String>,
{
    let Some(run_dir) = resolver(home) else {
        return Err("no active daemon (run dir not found)".to_string());
    };
    let mut arguments = serde_json::json!({
        "name": name,
        "backend": backend,
    });
    if !args.is_empty() {
        arguments["args"] = Value::String(args.join(" "));
    }
    if !env.is_empty() {
        arguments["env"] = serde_json::to_value(env)
            .map_err(|error| format!("could not encode instance environment: {error}"))?;
    }
    let result = caller(
        &run_dir,
        "create_instance",
        arguments,
        std::time::Duration::from_secs(60),
    )?;
    if let Some(error) = result.get("error").and_then(Value::as_str) {
        return Err(error.to_string());
    }
    result
        .get("name")
        .and_then(Value::as_str)
        .filter(|created| !created.is_empty())
        .map(String::from)
        .ok_or_else(|| "daemon create_instance returned no instance name".to_string())
}

pub(super) fn spawn_task_worker(
    home: &Path,
) -> (
    crossbeam_channel::Sender<TaskRequest>,
    crossbeam_channel::Receiver<TaskOutcome>,
    std::thread::JoinHandle<()>,
) {
    spawn_task_worker_inner(home, resolve_active_run_dir, call_tool_at)
}

fn spawn_task_worker_inner<R, C>(
    home: &Path,
    resolver: R,
    caller: C,
) -> (
    crossbeam_channel::Sender<TaskRequest>,
    crossbeam_channel::Receiver<TaskOutcome>,
    std::thread::JoinHandle<()>,
)
where
    R: Fn(&Path) -> Option<PathBuf> + Send + Sync + 'static,
    C: Fn(&Path, &str, Value, std::time::Duration) -> Result<Value, String> + Send + Sync + 'static,
{
    let (request_tx, request_rx) = crossbeam_channel::bounded(1);
    let (outcome_tx, outcome_rx) = crossbeam_channel::unbounded();
    let home = home.to_path_buf();
    // fire-and-forget: false; run_app joins the returned handle after dropping the sender.
    let worker = std::thread::Builder::new()
        .name("app-task-rpc".into())
        .spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let outcome = execute_with(&home, request, &resolver, &caller);
                if outcome_tx.send(outcome).is_err() {
                    break;
                }
            }
        })
        .expect("spawn app task RPC worker");
    (request_tx, outcome_rx, worker)
}

#[cfg(test)]
fn spawn_agent_state_worker_with_seams<R, C>(
    home: &Path,
    resolver: R,
    caller: C,
) -> (
    crossbeam_channel::Sender<AgentStateRequest>,
    crossbeam_channel::Receiver<AgentStateOutcome>,
    std::thread::JoinHandle<()>,
)
where
    R: Fn(&Path) -> Option<PathBuf> + Send + Sync + 'static,
    C: Fn(&Path, &str, Value, std::time::Duration) -> Result<Value, String> + Send + Sync + 'static,
{
    spawn_agent_state_worker_inner(home, resolver, caller)
}

pub(super) fn spawn_agent_state_worker(
    home: &Path,
) -> (
    crossbeam_channel::Sender<AgentStateRequest>,
    crossbeam_channel::Receiver<AgentStateOutcome>,
    std::thread::JoinHandle<()>,
) {
    spawn_agent_state_worker_inner(home, resolve_active_run_dir, call_tool_at)
}

fn spawn_agent_state_worker_inner<R, C>(
    home: &Path,
    resolver: R,
    caller: C,
) -> (
    crossbeam_channel::Sender<AgentStateRequest>,
    crossbeam_channel::Receiver<AgentStateOutcome>,
    std::thread::JoinHandle<()>,
)
where
    R: Fn(&Path) -> Option<PathBuf> + Send + Sync + 'static,
    C: Fn(&Path, &str, Value, std::time::Duration) -> Result<Value, String> + Send + Sync + 'static,
{
    let (request_tx, request_rx) = crossbeam_channel::bounded(1);
    let (outcome_tx, outcome_rx) = crossbeam_channel::bounded(1);
    let home = home.to_path_buf();
    // fire-and-forget: false; run_app joins the returned handle after dropping the sender.
    let worker = std::thread::Builder::new()
        .name("app-agent-state-rpc".into())
        .spawn(move || {
            while let Ok(AgentStateRequest::Refresh) = request_rx.recv() {
                let outcome = match resolver(&home) {
                    Some(run_dir) => list_instances_with_caller(&run_dir, &caller),
                    None => Err(AgentStateError {
                        error: "no active daemon (run dir not found)".to_string(),
                        mode: crate::runtime::AgentListMode::FallbackDaemonAbsent,
                    }),
                };
                if outcome_tx.send(outcome).is_err() {
                    break;
                }
            }
        })
        .expect("spawn app agent-state RPC worker");
    (request_tx, outcome_rx, worker)
}

fn execute_with<R, C>(home: &Path, request: TaskRequest, resolver: &R, caller: &C) -> TaskOutcome
where
    R: Fn(&Path) -> Option<PathBuf>,
    C: Fn(&Path, &str, Value, std::time::Duration) -> Result<Value, String>,
{
    let Some(run_dir) = resolver(home) else {
        return TaskOutcome::Failed {
            error: "no active daemon (run dir not found)".to_string(),
        };
    };
    match request {
        TaskRequest::List => match list_tasks_with_caller(&run_dir, caller) {
            Ok(snapshot) => TaskOutcome::Snapshot(snapshot),
            Err(error) => TaskOutcome::Failed { error },
        },
        TaskRequest::Mutate(arguments) => {
            if let Err(error) = call_task_with_caller(&run_dir, arguments, caller) {
                return TaskOutcome::MutationFailedUnknown {
                    error: format!("task write failed or timed out; outcome unknown: {error}"),
                };
            }
            match list_tasks_with_caller(&run_dir, caller) {
                Ok(snapshot) => TaskOutcome::MutationApplied { snapshot },
                Err(error) => TaskOutcome::MutationAppliedRefreshFailed { error },
            }
        }
    }
}

#[cfg(test)]
fn execute_with_seams<R, C>(
    home: &Path,
    request: TaskRequest,
    resolver: R,
    caller: C,
) -> TaskOutcome
where
    R: Fn(&Path) -> Option<PathBuf>,
    C: Fn(&Path, &str, Value, std::time::Duration) -> Result<Value, String>,
{
    execute_with(home, request, &resolver, &caller)
}

fn list_tasks_with_caller<C>(run_dir: &Path, caller: &C) -> Result<Vec<crate::tasks::Task>, String>
where
    C: Fn(&Path, &str, Value, std::time::Duration) -> Result<Value, String>,
{
    let result = call_task_with_caller(
        run_dir,
        serde_json::json!({"action": "list", "include_history": true, "verbose": true}),
        caller,
    )?;
    serde_json::from_value(result["tasks"].clone())
        .map_err(|error| format!("invalid task list from daemon: {error}"))
}

fn call_task_with_caller<C>(run_dir: &Path, arguments: Value, caller: &C) -> Result<Value, String>
where
    C: Fn(&Path, &str, Value, std::time::Duration) -> Result<Value, String>,
{
    caller(
        run_dir,
        "task",
        arguments,
        std::time::Duration::from_secs(10),
    )
}

fn list_instances_with_caller<C>(
    run_dir: &Path,
    caller: &C,
) -> Result<AgentStateSnapshotResult, AgentStateError>
where
    C: Fn(&Path, &str, Value, std::time::Duration) -> Result<Value, String>,
{
    let result = call_tool_with_caller(
        run_dir,
        "list_instances",
        serde_json::json!({}),
        std::time::Duration::from_secs(5),
        caller,
    )
    .map_err(|error| AgentStateError {
        error,
        mode: mode_after_rpc_failure(run_dir),
    })?;
    let instances = result["instances"]
        .as_array()
        .ok_or_else(|| AgentStateError {
            error: "invalid list_instances response: missing instances".to_string(),
            mode: mode_after_rpc_failure(run_dir),
        })?;
    let mut states = HashMap::new();
    let mut names = HashSet::new();
    let mut instance_refs = HashMap::new();
    for instance in instances {
        let Some(name) = instance["name"].as_str() else {
            continue;
        };
        names.insert(name.to_string());
        if let Ok(instance_ref) =
            serde_json::from_value::<crate::types::InstanceRef>(instance["instance_ref"].clone())
        {
            instance_refs.insert(name.to_string(), instance_ref);
        }
        states.insert(
            name.to_string(),
            instance["agent_state"].as_str().and_then(parse_agent_state),
        );
    }
    Ok(AgentStateSnapshotResult {
        snapshot: states,
        names,
        instance_refs,
        mode: crate::runtime::AgentListMode::Live,
    })
}

fn resolve_active_run_dir(home: &Path) -> Option<PathBuf> {
    crate::daemon::find_active_run_dir(home)
}

fn mode_after_rpc_failure(run_dir: &Path) -> crate::runtime::AgentListMode {
    // The old generation may disappear just after this check while its
    // successor is already live, so one pessimistic Fallback cycle is
    // possible; non-Live results remain roster-safe because they never
    // reconcile panes.
    if crate::daemon::read_daemon_pid(run_dir)
        .map(crate::process::is_pid_alive)
        .unwrap_or(false)
    {
        crate::runtime::AgentListMode::FallbackDaemonStuck
    } else {
        crate::runtime::AgentListMode::FallbackDaemonAbsent
    }
}

fn call_tool_at(
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

fn call_tool_with_caller<C>(
    run_dir: &Path,
    tool: &str,
    arguments: Value,
    timeout: std::time::Duration,
    caller: &C,
) -> Result<Value, String>
where
    C: Fn(&Path, &str, Value, std::time::Duration) -> Result<Value, String>,
{
    caller(run_dir, tool, arguments, timeout)
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
            move |_home: &std::path::Path| {
                resolved
                    .lock()
                    .expect("resolver mutex not poisoned")
                    .pop_front()
            }
        };
        let caller = {
            let calls = Arc::clone(&calls);
            move |run_dir: &std::path::Path,
                  _tool: &str,
                  _arguments: Value,
                  _timeout: std::time::Duration| {
                calls
                    .lock()
                    .expect("calls mutex not poisoned")
                    .push(run_dir.to_path_buf());
                let name = run_dir
                    .file_name()
                    .expect("test run directory has a name")
                    .to_string_lossy()
                    .to_string();
                Ok(serde_json::json!({
                    "instances": [{"name": name, "agent_state": "idle"}]
                }))
            }
        };
        let (_tx, rx, worker) = super::spawn_agent_state_worker_with_seams(
            std::path::Path::new("/home"),
            resolver,
            caller,
        );
        // The seam is request-driven: no wall-clock waits or daemon required.
        _tx.send(AgentStateRequest::Refresh)
            .expect("state worker request channel open");
        _tx.send(AgentStateRequest::Refresh)
            .expect("state worker request channel open");
        let first = rx
            .recv()
            .expect("state worker returned first outcome")
            .expect("first state refresh succeeded");
        let second = rx
            .recv()
            .expect("state worker returned second outcome")
            .expect("second state refresh succeeded");
        assert!(first.snapshot.contains_key("old-generation"));
        assert!(second.snapshot.contains_key("new-generation"));
        assert_eq!(
            &*calls.lock().expect("calls mutex not poisoned"),
            &[old, new]
        );
        drop(_tx);
        worker.join().expect("state worker joined cleanly");
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
                calls
                    .lock()
                    .expect("calls mutex not poisoned")
                    .push(tool.to_string());
                if calls.lock().expect("calls mutex not poisoned").len() == 1 {
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
        assert_eq!(
            &*calls.lock().expect("calls mutex not poisoned"),
            &["task", "task"]
        );
    }

    #[test]
    fn app_task_rpc_uses_the_bounded_cross_process_call() {
        let source = include_str!("rpc.rs");
        let production = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap_or(source);
        assert!(production.contains("crate::api::call_at("));
        assert!(production.contains("std::time::Duration::from_secs(10)"));
    }

    #[test]
    fn create_instance_rpc_forwards_name_backend_args_and_env() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let caller = {
            let calls = Arc::clone(&calls);
            move |run_dir: &std::path::Path,
                  tool: &str,
                  arguments: Value,
                  timeout: std::time::Duration| {
                calls.lock().expect("calls mutex not poisoned").push((
                    run_dir.to_path_buf(),
                    tool.to_string(),
                    arguments,
                    timeout,
                ));
                Ok(serde_json::json!({"name": "codex-created"}))
            }
        };
        let mut env = HashMap::new();
        env.insert("CODEX_TEST_FLAG".to_string(), "1".to_string());
        let created = super::create_instance_with(
            std::path::Path::new("/home"),
            "codex-requested",
            "codex",
            &["--model".to_string(), "gpt-test".to_string()],
            &env,
            |_home| Some(std::path::PathBuf::from("/run/current")),
            caller,
        )
        .expect("daemon create_instance succeeded");

        assert_eq!(created, "codex-created");
        let calls = calls.lock().expect("calls mutex not poisoned");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, std::path::Path::new("/run/current"));
        assert_eq!(calls[0].1, "create_instance");
        assert_eq!(calls[0].3, std::time::Duration::from_secs(60));
        assert_eq!(
            calls[0].2,
            serde_json::json!({
                "name": "codex-requested",
                "backend": "codex",
                "args": "--model gpt-test",
                "env": {"CODEX_TEST_FLAG": "1"}
            })
        );
    }

    #[test]
    fn restart_instance_rpc_forwards_exact_name_and_resume_mode() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let caller = {
            let calls = Arc::clone(&calls);
            move |run_dir: &std::path::Path,
                  tool: &str,
                  arguments: Value,
                  timeout: std::time::Duration| {
                calls.lock().expect("calls mutex not poisoned").push((
                    run_dir.to_path_buf(),
                    tool.to_string(),
                    arguments,
                    timeout,
                ));
                Ok(serde_json::json!({"spawned": true, "tui_handoff": true}))
            }
        };

        super::restart_instance_with(
            std::path::Path::new("/home"),
            "fleet-agent",
            |_home| Some(std::path::PathBuf::from("/run/current")),
            caller,
        )
        .expect("daemon restart_instance succeeded");

        let calls = calls.lock().expect("calls mutex not poisoned");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, std::path::Path::new("/run/current"));
        assert_eq!(calls[0].1, "restart_instance");
        assert_eq!(calls[0].3, std::time::Duration::from_secs(60));
        assert_eq!(
            calls[0].2,
            serde_json::json!({
                "instance": "fleet-agent",
                "mode": "resume",
                "reason": "manual TUI :restart",
            })
        );
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
