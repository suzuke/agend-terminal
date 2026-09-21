use serde_json::{json, Value};
use std::path::Path;

#[cfg(test)]
pub(crate) fn handle_delete_instance(
    home: &Path,
    args: &Value,
    sender: &Option<crate::identity::Sender>,
) -> Value {
    handle_delete_instance_with_runtime(home, args, sender, None)
}

pub(crate) fn handle_delete_instance_with_runtime(
    home: &Path,
    args: &Value,
    sender: &Option<crate::identity::Sender>,
    runtime: Option<&super::super::dispatch::RuntimeContext>,
) -> Value {
    let name = match super::super::require_instance(args) {
        Ok(n) => n,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(name);
    // AUDIT2-002: deleting an instance tears down its PTY, inbox and worktree and
    // orphans its tasks. Restrict an identified caller to deleting itself or a
    // member of a team it orchestrates — a peer can no longer remove another
    // agent by naming it. Anonymous (no sender: operator-direct / standalone)
    // keeps full authority for explicit local/operator lifecycle calls.
    //
    // ACL improvement: also allow the instance's CREATOR (the caller that ran
    // `create_instance` for it, stamped as `created_by` in fleet.yaml — the
    // "為 ACL 建 team" pain point: a creator wanting to redo/retire its own
    // spawn shouldn't have to build a team just to gain orchestrator
    // authority). Guarded by an in-flight safety valve: if the target has an
    // active worktree binding or a claimed/in_progress task, the creator path
    // requires `force=true` + a non-empty `force_reason` (audit-logged), so a
    // creator can't casually reap an agent mid-work. Self/orchestrator deletes
    // are unaffected by the valve — this only gates the NEW creator path.
    if let Some(caller) = sender.as_ref().map(|s| s.as_str()) {
        if caller != name && !crate::teams::is_orchestrator_of(home, caller, name) {
            let is_creator = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                .ok()
                .and_then(|c| c.instances.get(name).and_then(|i| i.created_by.clone()))
                .as_deref()
                == Some(caller);
            if !is_creator {
                return serde_json::json!({
                    "error": format!(
                        "permission denied: '{caller}' cannot delete '{name}' \
                         (only the instance itself, its team orchestrator, or its creator may)"
                    ),
                    "code": "not_owner_or_orchestrator"
                });
            }
            let has_binding = crate::binding::read(home, name).is_some();
            let has_active_task = crate::tasks::list_all(home).iter().any(|t| {
                t.assignee.as_deref() == Some(name)
                    && matches!(
                        t.status,
                        crate::task_events::TaskStatus::Claimed
                            | crate::task_events::TaskStatus::InProgress
                    )
            });
            if has_binding || has_active_task {
                let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
                let force_reason = args
                    .get("force_reason")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                match force_reason {
                    Some(reason) if force => {
                        tracing::warn!(
                            caller,
                            target = name,
                            reason,
                            has_binding,
                            has_active_task,
                            "creator force-deleting instance with in-flight work"
                        );
                        // Durable audit trail — a permission override deleting
                        // in-flight work needs more than a process log line.
                        // The helper remains in the parent module so the audit
                        // sink invariant can inspect this production region.
                        if let Err(e) = super::record_creator_force_delete(
                            home,
                            caller,
                            name,
                            reason,
                            has_binding,
                            has_active_task,
                        ) {
                            return serde_json::json!({
                                "error": format!("creator force-delete refused: {e}"),
                                "code": "creator_force_delete_audit_failed"
                            });
                        }
                    }
                    _ => {
                        return serde_json::json!({
                            "error": format!(
                                "'{name}' has in-flight work (binding={has_binding}, \
                                 active_task={has_active_task}) — creator delete requires \
                                 force=true and a non-empty force_reason"
                            ),
                            "code": "creator_delete_requires_force"
                        });
                    }
                }
            }
        }
    }
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
    if let Some(ref config) = fleet {
        if config.channel.is_some()
            && config.instances.contains_key(name)
            && config.instances.len() <= 1
        {
            return json!({"error": "cannot delete the last instance — channel needs at least one instance to receive messages"});
        }
    }
    // Full multi-store teardown lives in the `lifecycle` submodule of this
    // `instance_state` concept (Sprint 54 P1-B Bug 1).
    let delete_context = runtime.map(|runtime| crate::agent_ops::DeleteContext {
        registry: &runtime.registry,
        configs: &runtime.configs,
        externals: &runtime.externals,
        notifier: runtime.notifier.as_ref(),
    });
    match super::lifecycle::full_delete_instance_with_runtime(home, name, delete_context.as_ref()) {
        Ok(()) => json!({"name": name}),
        Err(detail) => {
            let recovery_required = detail.starts_with("recovery_required:");
            let mut result = json!({
                "name": name,
                "error": if recovery_required {
                    format!("delete requires operator recovery: {detail}")
                } else {
                    format!(
                        "delete completed with residual state — fleet may resurrect on next reconcile: {detail}"
                    )
                },
            });
            if recovery_required {
                result["code"] = json!("recovery_required");
            }
            result
        }
    }
}
