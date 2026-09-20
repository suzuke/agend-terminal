use serde_json::Value;
use std::path::Path;

pub(super) fn handle_create_schedule(home: &Path, args: &Value, instance_name: &str) -> Value {
    crate::schedules::create(home, instance_name, args)
}

pub(super) fn handle_list_schedules(home: &Path, args: &Value) -> Value {
    crate::schedules::list(home, args)
}

pub(super) fn handle_update_schedule(home: &Path, args: &Value) -> Value {
    crate::schedules::update(home, args)
}

pub(super) fn handle_delete_schedule(home: &Path, args: &Value) -> Value {
    crate::schedules::delete(home, args)
}

pub(super) fn handle_deploy_template(
    home: &Path,
    args: &Value,
    instance_name: &str,
    runtime: Option<&crate::deployments::DeploymentRuntime<'_>>,
) -> Value {
    crate::deployments::deploy_with_runtime(home, instance_name, args, runtime)
}

pub(super) fn handle_teardown_deployment(
    home: &Path,
    args: &Value,
    runtime: Option<&crate::deployments::DeploymentRuntime<'_>>,
) -> Value {
    crate::deployments::teardown_with_runtime(home, args, runtime)
}

pub(super) fn handle_list_deployments(home: &Path) -> Value {
    crate::deployments::list(home)
}

pub(super) fn handle_job_runs(home: &Path, args: &Value) -> Value {
    crate::schedule_jobs::list(home, args["id"].as_str())
}
pub(super) fn handle_job_complete(home: &Path, args: &Value, instance_name: &str) -> Value {
    crate::schedule_jobs::complete(home, instance_name, args)
}

pub(super) fn handle_job_deliver(home: &Path, args: &Value, instance_name: &str) -> Value {
    let content = if let Some(path) = args["message_from_file"].as_str().filter(|s| !s.is_empty()) {
        match super::read_message_file(path) {
            Ok(content) => content,
            Err(error) => return serde_json::json!({"error": error}),
        }
    } else {
        match args["message"].as_str() {
            Some(text) if !text.is_empty() => text.to_string(),
            _ => {
                return serde_json::json!({
                    "error": "missing 'message' or 'message_from_file'"
                })
            }
        }
    };
    crate::schedule_jobs::deliver(home, instance_name, args, &content)
}

pub(super) fn handle_job_resolve_recovery(home: &Path, args: &Value) -> Value {
    crate::schedule_jobs::resolve_recovery(home, args)
}
