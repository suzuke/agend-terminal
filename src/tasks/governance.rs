//! Typed task/decision review authority.
//!
//! This module is deliberately shared by task creation and every PR-producing
//! consumer. The task event is the durable boundary: after creation, the two
//! authority keys are never writable through generic metadata.

use crate::daemon::pr_state::ReviewClass;
use crate::task_events::TaskRecord;
use serde_json::{json, Value};
use std::path::Path;

#[cfg(test)]
use std::sync::{Mutex, OnceLock};

#[cfg(test)]
type AuthorityResolvedHook = Box<dyn FnOnce(&Path) + Send + 'static>;

#[cfg(test)]
static AUTHORITY_RESOLVED_HOOK: OnceLock<Mutex<Option<AuthorityResolvedHook>>> = OnceLock::new();

#[derive(Debug, Clone)]
pub(crate) struct TaskCreationAuthority {
    pub governing_decision_id: Option<String>,
    pub review_class: Option<ReviewClass>,
    /// Invalid legacy explicit values remain visible to old task consumers;
    /// governed creation never uses this escape hatch and rejects instead.
    pub legacy_review_class_raw: Option<String>,
}

fn requested_governing_decision_id(args: &Value) -> Result<Option<&str>, Value> {
    let Some(value) = args.get("governing_decision_id") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(raw) = value.as_str() else {
        return Err(json!({
            "error": "governing_decision_id must be a non-empty string",
            "code": "invalid_governing_decision_id",
        }));
    };
    let id = raw.trim();
    if id.is_empty() {
        return Err(json!({
            "error": "governing_decision_id must be a non-empty string",
            "code": "invalid_governing_decision_id",
        }));
    }
    Ok(Some(id))
}

/// Hold the exact governing decision flock from before authority resolution
/// through the caller's Created append. The lock order is decision flock →
/// task-id flock → board-event flock; no decision-store I/O is performed while
/// the board lock is held.
pub(crate) fn acquire_creation_decision_lock(
    home: &Path,
    args: &Value,
) -> Result<Option<crate::store::FileFlockGuard>, Value> {
    let Some(id) = requested_governing_decision_id(args)? else {
        return Ok(None);
    };
    crate::decisions::acquire_decision_lock(home, id)
        .map(Some)
        .map_err(|error| {
            json!({
                "error": format!("could not lock governing decision '{id}': {error}"),
                "code": "invalid_governing_decision_id",
                "governing_decision_id": id,
            })
        })
}

#[cfg(test)]
pub(crate) fn install_authority_resolved_hook(hook: AuthorityResolvedHook) {
    let hooks = AUTHORITY_RESOLVED_HOOK.get_or_init(|| Mutex::new(None));
    let mut slot = hooks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = Some(hook);
}

#[cfg(test)]
fn fire_authority_resolved_hook(home: &Path) {
    let Some(hooks) = AUTHORITY_RESOLVED_HOOK.get() else {
        return;
    };
    let hook = hooks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(hook) = hook {
        hook(home);
    }
}

fn explicit_review_class(args: &Value) -> Result<(Option<ReviewClass>, Option<String>), Value> {
    let Some(value) = args.get("review_class") else {
        return Ok((None, None));
    };
    if value.is_null() {
        return Ok((None, None));
    }
    let Some(raw) = value.as_str() else {
        return Err(json!({
            "error": "review_class must be a string ('single' or 'dual')",
            "code": "invalid_review_class",
        }));
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok((None, None));
    }
    match ReviewClass::parse_fail_closed(Some(raw)) {
        ReviewClass::Single => Ok((Some(ReviewClass::Single), None)),
        ReviewClass::Dual => Ok((Some(ReviewClass::Dual), None)),
        ReviewClass::Unresolved => Ok((None, Some(raw.to_string()))),
    }
}

/// Resolve create-time decision inheritance and class consistency before any
/// task id, board route, supersession, or event-log side effect.
pub(crate) fn resolve_creation_authority(
    home: &Path,
    args: &Value,
) -> Result<TaskCreationAuthority, Value> {
    let (explicit_class, legacy_raw) = explicit_review_class(args)?;
    let Some(decision_id) = requested_governing_decision_id(args)? else {
        return Ok(TaskCreationAuthority {
            governing_decision_id: None,
            review_class: explicit_class,
            legacy_review_class_raw: legacy_raw,
        });
    };
    if legacy_raw.is_some() {
        return Err(json!({
            "error": "governed task review_class must be exactly 'single' or 'dual'",
            "code": "governing_decision_review_class_invalid",
        }));
    }
    let decision =
        crate::decisions::resolve_governing_decision(home, decision_id).map_err(|e| {
            json!({
                "error": format!("governing decision '{decision_id}' could not be resolved: {e}"),
                "code": "governing_decision_unresolved",
                "governing_decision_id": decision_id,
            })
        })?;
    if let (Some(decision_class), Some(task_class)) = (decision.review_class, explicit_class) {
        if decision_class != task_class {
            return Err(json!({
                "error": format!(
                    "governing decision review_class={} conflicts with task review_class={}",
                    decision_class.as_token(),
                    task_class.as_token()
                ),
                "code": "governing_decision_review_class_mismatch",
                "governing_decision_id": decision_id,
            }));
        }
    }
    let authority = TaskCreationAuthority {
        governing_decision_id: Some(decision_id.to_string()),
        review_class: explicit_class.or(decision.review_class),
        legacy_review_class_raw: None,
    };
    #[cfg(test)]
    fire_authority_resolved_hook(home);
    Ok(authority)
}

fn metadata_class(record: &TaskRecord) -> Result<Option<ReviewClass>, Value> {
    let Some(raw) = record.metadata.get("review_class") else {
        return Ok(None);
    };
    let Some(raw) = raw.as_str() else {
        return Err(json!({
            "error": "task review_class metadata is not a string",
            "code": "task_governance_tampered",
        }));
    };
    match ReviewClass::parse_fail_closed(Some(raw)) {
        ReviewClass::Single => Ok(Some(ReviewClass::Single)),
        ReviewClass::Dual => Ok(Some(ReviewClass::Dual)),
        ReviewClass::Unresolved => Err(json!({
            "error": "task review_class metadata is invalid",
            "code": "task_governance_tampered",
        })),
    }
}

/// Validate the durable task authority, including a fresh exact decision
/// reload. A task with a governing decision must carry a matching resolved
/// class whenever the decision declares one.
pub(crate) fn validate_existing_authority(
    home: &Path,
    task_id: &str,
    record: &TaskRecord,
) -> Result<Option<ReviewClass>, Value> {
    let task_class = metadata_class(record)?;
    let Some(raw_decision_id) = record.metadata.get("governing_decision_id") else {
        return Ok(task_class);
    };
    let Some(decision_id) = raw_decision_id.as_str().filter(|id| !id.trim().is_empty()) else {
        return Err(json!({
            "error": format!("task '{task_id}' governing_decision_id is invalid"),
            "code": "task_governance_tampered",
        }));
    };
    let decision =
        crate::decisions::resolve_governing_decision(home, decision_id).map_err(|e| {
            json!({
                "error": format!("task '{task_id}' governing decision could not be resolved: {e}"),
                "code": "task_governing_decision_unresolved",
                "task_id": task_id,
                "governing_decision_id": decision_id,
            })
        })?;
    if decision.review_class.is_some() && task_class != decision.review_class {
        return Err(json!({
            "error": format!("task '{task_id}' governing decision and review_class metadata diverge"),
            "code": "task_governance_tampered",
            "task_id": task_id,
            "governing_decision_id": decision_id,
        }));
    }
    Ok(task_class.or(decision.review_class))
}

/// Resolve the class for a branch-carrying dispatch. Existing tasks are
/// authoritative; an auto-created task may inherit from its named decision.
/// The returned class is the only value permitted to flow into CI watch/PrState.
pub(crate) fn resolve_dispatch_authority(
    home: &Path,
    task_id: &str,
    args: &Value,
    second_reviewer: bool,
) -> Result<ReviewClass, Value> {
    if task_id.is_empty() {
        let authority = resolve_creation_authority(home, args)?;
        let Some(class) = authority.review_class else {
            return Err(json!({
                "error": "review_class unspecified for branch dispatch",
                "code": "review_class_unspecified",
            }));
        };
        if second_reviewer && class == ReviewClass::Single {
            return Err(json!({
                "error": "second_reviewer=true conflicts with review_class=single",
                "code": "review_class_mismatch",
            }));
        }
        return Ok(class);
    }

    let routed = match crate::tasks::load_routed(home, task_id) {
        Ok(routed) => routed,
        Err(crate::tasks::TaskRouteError::NotFound) => {
            return Err(json!({
                "error": format!("task '{task_id}' was not found"),
                "code": "review_class_route_unresolved",
            }))
        }
        Err(error) => {
            return Err(json!({
                "error": format!("task '{task_id}' route is unresolved: {error}"),
                "code": "review_class_route_unresolved",
            }))
        }
    };
    let record = routed.record();
    let Some(class) = validate_existing_authority(home, task_id, record)? else {
        return Err(json!({
            "error": "review_class unspecified for existing branch task",
            "code": "review_class_unspecified",
            "task_id": task_id,
        }));
    };
    if let Some(value) = args.get("review_class").filter(|value| !value.is_null()) {
        let Some(raw) = value.as_str() else {
            return Err(json!({
                "error": "dispatch review_class must be exactly 'single' or 'dual'",
                "code": "review_class_invalid",
                "task_id": task_id,
            }));
        };
        if ReviewClass::parse_fail_closed(Some(raw)) != class {
            return Err(json!({
                "error": format!("task review_class={} conflicts with dispatch review_class", class.as_token()),
                "code": "review_class_mismatch",
                "task_id": task_id,
            }));
        }
    }
    if second_reviewer && class == ReviewClass::Single {
        return Err(json!({
            "error": "second_reviewer=true conflicts with review_class=single",
            "code": "review_class_mismatch",
            "task_id": task_id,
        }));
    }
    Ok(class)
}
