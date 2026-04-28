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

pub(super) fn handle_deploy_template(home: &Path, args: &Value, instance_name: &str) -> Value {
    crate::deployments::deploy(home, instance_name, args)
}

pub(super) fn handle_teardown_deployment(home: &Path, args: &Value) -> Value {
    crate::deployments::teardown(home, args)
}

pub(super) fn handle_list_deployments(home: &Path) -> Value {
    crate::deployments::list(home)
}
